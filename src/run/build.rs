//! Build responsibilities for run.

use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use anyhow::bail;
use phoxal::model::robot::v0::ConnectionConfig;
use phoxal_cli_core::project::resolver::ResolvedRobot;
use phoxal_cli_core::project::tooling::{cargo_binary_name, cargo_package_name};
use serde_json::Value;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

/// How a staging pass produces the workspace user/driver crate binaries it
/// links into the runtime layout's flat `bin/` store (#936).
///
/// `run`, `start`, `watch`, and a host `phoxal build` use [`StagingBuild::Local`],
/// a `cargo build` on this host, optionally cross-compiling to a `--target`.
/// The `container` builder compiles the workspace inside a toolchain image first
/// and then reuses the identical host-side staging with
/// [`StagingBuild::Prebuilt`], which points the same lookup at the binaries the
/// container already produced under a mounted target directory. Both feed
/// `stage_complete_bin_store`, so the layout, validation, and archive are one
/// shared implementation regardless of where compilation happened.
#[derive(Debug, Clone)]
pub(crate) enum StagingBuild {
    /// Build user/driver crates on this host with `cargo build`, cross-compiling
    /// to `target` when it is set and differs from the host.
    Local { target: Option<String> },
    /// Reuse binaries already built (by the container builder) under
    /// `target_dir` - the cargo target directory of the container's snapshot -
    /// for `target`. No cargo runs on the host for these.
    Prebuilt {
        target: Option<String>,
        target_dir: PathBuf,
    },
}

impl StagingBuild {
    /// A host build for the given cross target (`None` = host triple).
    pub(crate) fn local(target: Option<String>) -> Self {
        Self::Local { target }
    }

    /// The requested target triple, or `None` for a host-native staging pass.
    pub(crate) fn target(&self) -> Option<&str> {
        match self {
            Self::Local { target } | Self::Prebuilt { target, .. } => target.as_deref(),
        }
    }

    /// Produce one workspace user/driver crate binary for this staging pass.
    pub(crate) fn build_user_binary(
        &self,
        crate_dir: &Path,
        preferred_name: &str,
        ui: &crate::Ui,
    ) -> Result<PathBuf> {
        match self {
            Self::Local { target } => {
                build_source_binary(crate_dir, preferred_name, ui, target.as_deref())
            }
            Self::Prebuilt { target, target_dir } => {
                locate_prebuilt_binary(crate_dir, preferred_name, target_dir, target.as_deref())
            }
        }
    }
}

/// Build one source participant while routing captured output through the
/// session. `target` cross-compiles with `cargo build --target <TRIPLE>` when it
/// is set and differs from the host; a missing cross toolchain fails with an
/// actionable `rustup target add` error rather than an opaque cargo failure.
pub(crate) fn build_source_binary(
    crate_dir: &Path,
    preferred_name: &str,
    ui: &crate::Ui,
    target: Option<&str>,
) -> Result<PathBuf> {
    let crate_dir = crate_dir.canonicalize().with_context(|| {
        format!(
            "failed to canonicalize source crate {}",
            crate_dir.display()
        )
    })?;
    let binary_name = cargo_binary_name(&crate_dir, Some(preferred_name))?;
    let package_name = cargo_package_name(&crate_dir)?;
    // Only cross-compile when the target genuinely differs from the host: an
    // explicit `--target <host-triple>` reuses the plain `target/debug` output
    // exactly as `run` does, so a host `build` and a `run` share the incremental
    // cache instead of a redundant `target/<host-triple>/debug`.
    let cross = target.filter(|triple| *triple != crate::resolver::host_target_triple());
    if let Some(triple) = cross {
        ensure_cross_toolchain(triple)?;
    }
    let cargo_target_flag = cross.map(str::to_string);
    ui.info(format!(
        "building user participant {preferred_name} with cargo build -p {package_name} --bin {binary_name}{}",
        cargo_target_flag
            .as_deref()
            .map(|triple| format!(" --target {triple}"))
            .unwrap_or_default()
    ));
    // Finding A3: a source participant only ever gets here when it genuinely
    // needs a fresh `cargo build` (path-overridden components/simulators, or
    // any user service/driver built from local source) - so bracketing this
    // exact call with a "build" phase reports truthful per-operation work
    // rather than the old synthetic single "Preparing" phase.
    crate::session::diagnostics::run_phase(
        phoxal_cli_core::session::event::PhaseId::new("build"),
        format!("Building {preferred_name}"),
        || {
            let mut command = Command::new("cargo");
            command
                .arg("build")
                .arg("-p")
                .arg(&package_name)
                .arg("--bin")
                .arg(&binary_name)
                .current_dir(&crate_dir);
            if let Some(triple) = cross {
                command.arg("--target").arg(triple);
            }
            let status = ui.command_status_captured(&mut command).with_context(|| {
                format!(
                    "failed to start cargo build for participant {preferred_name} in {}",
                    crate_dir.display()
                )
            })?;
            if !status.success() {
                bail!(
                    "cargo build failed for participant {preferred_name} in {} with status {status}",
                    crate_dir.display()
                );
            }
            Ok(())
        },
    )?;
    Ok(debug_binary_path(
        &cargo_target_dir(&crate_dir)?,
        cross,
        &binary_name,
    ))
}

/// Resolve a user/driver crate binary the container builder already compiled
/// into `target_dir` (the container snapshot's cargo target directory), for the
/// same `target` a host build would have used. No cargo runs here - the binary
/// must already exist, so a missing one is a precise error naming it.
fn locate_prebuilt_binary(
    crate_dir: &Path,
    preferred_name: &str,
    target_dir: &Path,
    target: Option<&str>,
) -> Result<PathBuf> {
    let binary_name = cargo_binary_name(crate_dir, Some(preferred_name))?;
    let cross = target.filter(|triple| *triple != crate::resolver::host_target_triple());
    let path = debug_binary_path(target_dir, cross, &binary_name);
    if !path.is_file() {
        bail!(
            "container build did not produce the binary for `{preferred_name}` (expected {}); \
             the in-container `cargo build --target` may have failed",
            path.display()
        );
    }
    Ok(path)
}

/// The `debug` build output path for `binary_name` under `target_dir`, in the
/// `<triple>/debug/` subtree when cross-compiling and plain `debug/` otherwise.
fn debug_binary_path(target_dir: &Path, cross: Option<&str>, binary_name: &str) -> PathBuf {
    let debug = match cross {
        Some(triple) => target_dir.join(triple).join("debug"),
        None => target_dir.join("debug"),
    };
    debug.join(binary_name_with_suffix(binary_name))
}

/// Fail with an actionable error when the cross target's standard library is not
/// installed, naming the exact `rustup target add` command. The CLI never
/// installs toolchains (#936); this only turns an opaque later cargo failure
/// into a precise instruction. If `rustup` is not on PATH the check is skipped -
/// a non-rustup toolchain may still have the target - and cargo reports any real
/// gap itself.
fn ensure_cross_toolchain(triple: &str) -> Result<()> {
    let output = Command::new("rustup")
        .args(["target", "list", "--installed"])
        .output();
    let Ok(output) = output else {
        // No rustup on PATH: let the build proceed and cargo surface any gap.
        return Ok(());
    };
    if !output.status.success() {
        return Ok(());
    }
    let installed = String::from_utf8_lossy(&output.stdout);
    if installed.lines().any(|line| line.trim() == triple) {
        return Ok(());
    }
    bail!(
        "the Rust standard library for target `{triple}` is not installed; \
         install it with `rustup target add {triple}` (the CLI never installs toolchains), \
         or use `--builder container` to compile inside a toolchain image"
    )
}

pub(crate) fn cargo_target_dir(crate_dir: &Path) -> Result<PathBuf> {
    let output = crate::shell::run_stdout(
        "cargo",
        ["metadata", "--format-version", "1", "--no-deps"],
        Some(crate_dir),
    )?;
    let json: Value = serde_json::from_str(&output).context("cargo metadata was not JSON")?;
    json.get("target_directory")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .ok_or_else(|| anyhow!("cargo metadata did not include target_directory"))
}

pub(crate) fn binary_name_with_suffix(binary_name: &str) -> String {
    if cfg!(windows) {
        format!("{binary_name}.exe")
    } else {
        binary_name.to_string()
    }
}

pub(crate) fn device_missing_note(
    resolved: &ResolvedRobot,
    participant_id: &str,
) -> Option<String> {
    let component = resolved.robot.robot.components.get(participant_id)?;
    let driver = component.driver.as_ref()?;
    let missing = missing_device_path(&driver.connection)?;
    Some(format!(
        "DeviceMissing: {missing} for driver {participant_id}"
    ))
}

pub(crate) fn missing_device_path(connection: &ConnectionConfig) -> Option<String> {
    match connection {
        ConnectionConfig::Serial { port, .. } | ConnectionConfig::Uart { port, .. } => {
            (!Path::new(port).exists()).then(|| port.clone())
        }
        ConnectionConfig::Can { bus, .. } => {
            let path = PathBuf::from(format!("/sys/class/net/can{bus}"));
            (!path.exists()).then(|| path.display().to_string())
        }
        ConnectionConfig::I2c { bus, .. } => {
            let path = PathBuf::from(format!("/dev/i2c-{bus}"));
            (!path.exists()).then(|| path.display().to_string())
        }
        ConnectionConfig::Spi { bus, chip_select } => {
            let path = PathBuf::from(format!("/dev/spidev{bus}.{chip_select}"));
            (!path.exists()).then(|| path.display().to_string())
        }
        ConnectionConfig::Gpio { chip, .. } => {
            let path = if chip.starts_with('/') {
                PathBuf::from(chip)
            } else {
                PathBuf::from("/dev").join(chip)
            };
            (!path.exists()).then(|| path.display().to_string())
        }
        ConnectionConfig::Usb {
            vendor_id,
            product_id,
        } => usb_missing(*vendor_id, *product_id),
    }
}

pub(crate) fn usb_missing(vendor_id: Option<u16>, product_id: Option<u16>) -> Option<String> {
    let (Some(vendor_id), Some(product_id)) = (vendor_id, product_id) else {
        return None;
    };
    let devices = Path::new("/sys/bus/usb/devices");
    let entries = fs::read_dir(devices).ok()?;
    let wanted_vendor = format!("{vendor_id:04x}");
    let wanted_product = format!("{product_id:04x}");
    for entry in entries.flatten() {
        let path = entry.path();
        let vendor = fs::read_to_string(path.join("idVendor")).unwrap_or_default();
        let product = fs::read_to_string(path.join("idProduct")).unwrap_or_default();
        if vendor.trim().eq_ignore_ascii_case(&wanted_vendor)
            && product.trim().eq_ignore_ascii_case(&wanted_product)
        {
            return None;
        }
    }
    Some(format!("usb {wanted_vendor}:{wanted_product}"))
}
