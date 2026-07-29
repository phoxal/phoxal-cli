//! Build responsibilities for run.

use super::profile::Profile;
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

/// Build one source participant while routing captured output through the
/// session. `target` cross-compiles with `cargo build --target <TRIPLE>` when it
/// is set and differs from the host; a missing cross toolchain fails with an
/// actionable `rustup target add` error rather than an opaque cargo failure.
/// Builds `debug` for the fast interactive `run`/`start`/simulation loop.
/// Deployable bundles call [`build_source_binary_with_profile`] with release
/// explicitly.
pub(crate) fn build_source_binary(
    crate_dir: &Path,
    preferred_name: &str,
    ui: &dyn crate::Reporter,
    target: Option<&str>,
    offline: bool,
) -> Result<PathBuf> {
    build_source_binary_with_profile(
        crate_dir,
        preferred_name,
        ui,
        target,
        Profile::Debug,
        offline,
    )
}

/// [`build_source_binary`] with an explicit build profile. `offline` appends
/// `--offline` to the underlying `cargo build` (organization#951 WS4 review,
/// round 2): when the caller asked the whole CLI invocation to stay offline,
/// that must hold for a user's own crate exactly as much as for official
/// materialization - a build that quietly went online anyway despite
/// `--offline` would be a real behavioral gap, not a convenience. If the
/// local registry/git caches are not already warm, Cargo itself fails with
/// its own precise offline error; that failure is honest, a silent network
/// call would not have been.
pub(crate) fn build_source_binary_with_profile(
    crate_dir: &Path,
    preferred_name: &str,
    ui: &dyn crate::Reporter,
    target: Option<&str>,
    profile: Profile,
    offline: bool,
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
    let cross = target.filter(|triple| *triple != crate::resolve::project::host_target_triple());
    if let Some(triple) = cross {
        ensure_cross_toolchain(triple)?;
    }
    let cargo_target_flag = cross.map(str::to_string);
    ui.info(format!(
        "building user participant {preferred_name} with cargo build -p {package_name} --bin {binary_name}{}{}",
        cargo_target_flag
            .as_deref()
            .map(|triple| format!(" --target {triple}"))
            .unwrap_or_default(),
        matches!(profile, Profile::Release)
            .then_some(" --release")
            .unwrap_or_default()
    ));
    // Finding A3: a source participant only ever gets here when it genuinely
    // needs a fresh `cargo build` (path-overridden components/simulators, or
    // any user service/driver built from local source) - so bracketing this
    // exact call with a "build" phase reports truthful per-operation work
    // rather than the old synthetic single "Preparing" phase.
    crate::progress::run_phase(
        ui,
        crate::PhaseId::new("build"),
        format!("Building {preferred_name}"),
        || {
            let mut command = Command::new("cargo");
            // `--locked` pins the build to the committed `Cargo.lock`: staging
            // is reproducible and cargo never silently rewrites the lock, so a
            // stale or missing lock is a hard, actionable error instead of a
            // quiet resolve. `--offline` is threaded from the caller, exactly
            // like every other Cargo invocation this CLI makes (see
            // [`build_source_binary_with_profile`]'s doc comment).
            command
                .arg("build")
                .arg("--locked")
                .arg("-p")
                .arg(&package_name)
                .arg("--bin")
                .arg(&binary_name)
                .current_dir(&crate_dir);
            if let Some(triple) = cross {
                command.arg("--target").arg(triple);
            }
            if matches!(profile, Profile::Release) {
                command.arg("--release");
            }
            if offline {
                command.arg("--offline");
            }
            let status = crate::build::shell::command_status_captured(&mut command, ui)
                .with_context(|| {
                    format!(
                        "failed to start cargo build for participant {preferred_name} in {}",
                        crate_dir.display()
                    )
                })?;
            if !status.success() {
                bail!(
                    "cargo build (--locked) failed for participant {preferred_name} in {} with status {status}; \
                     if this is a lockfile mismatch, run `cargo update` (or `cargo generate-lockfile`) in the project and commit the result",
                    crate_dir.display()
                );
            }
            Ok(())
        },
    )?;
    Ok(profile_binary_path(
        &cargo_target_dir(&crate_dir, offline)?,
        cross,
        profile,
        &binary_name,
    ))
}

/// Resolve a user/driver crate binary the container builder already compiled
/// into `target_dir` (the project's persistent Cargo target directory,
/// bind-mounted into the container at `/phoxal/src/target`), for the same
/// `target` a host build would have used. No cargo runs here - the binary must
/// already exist, so a missing one is a precise error naming it.
pub(super) fn locate_prebuilt_binary(
    crate_dir: &Path,
    preferred_name: &str,
    target_dir: &Path,
    target: Option<&str>,
    profile: Profile,
) -> Result<PathBuf> {
    let binary_name = cargo_binary_name(crate_dir, Some(preferred_name))?;
    // The container always compiles with an explicit `--target <triple>`, so
    // its output is always under `target/<triple>/<profile>` - including when the
    // triple equals the CLI host triple (a Linux host building its own arch in
    // a container). Never collapse to the implicit `target/debug` here (#936).
    let path = profile_binary_path(target_dir, target, profile, &binary_name);
    if !path.is_file() {
        bail!(
            "container build did not produce the binary for `{preferred_name}` (expected {}); \
             the in-container `cargo build --target` may have failed",
            path.display()
        );
    }
    Ok(path)
}

/// The build output path for `binary_name` under `target_dir` for `profile`,
/// in the `<triple>/<profile>/` subtree when cross-compiling and plain
/// `<profile>/` otherwise.
fn profile_binary_path(
    target_dir: &Path,
    cross: Option<&str>,
    profile: Profile,
    binary_name: &str,
) -> PathBuf {
    let dir = match cross {
        Some(triple) => target_dir.join(triple).join(profile.dir_name()),
        None => target_dir.join(profile.dir_name()),
    };
    dir.join(binary_name_with_suffix(binary_name))
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

pub(crate) fn cargo_target_dir(crate_dir: &Path, offline: bool) -> Result<PathBuf> {
    // `--locked` keeps metadata reads on the committed `Cargo.lock` too, so
    // resolving the target directory never triggers a lock rewrite or a
    // registry resolve. `--no-deps` already means this call has no real need
    // to reach the network, but `--offline` is threaded through anyway
    // (organization#951 WS4 review, round 2) so it holds on principle for
    // every Cargo invocation this CLI makes, not just the ones that happen
    // to need it today.
    let mut args = vec!["metadata", "--format-version", "1", "--no-deps", "--locked"];
    if offline {
        args.push("--offline");
    }
    let output = crate::build::shell::run_stdout("cargo", args, Some(crate_dir), None)?;
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

#[cfg(test)]
mod prebuilt_tests {
    use super::*;

    /// The container always compiles with an explicit `--target`, so the
    /// prebuilt lookup must read `target/<triple>/release` even when the triple
    /// equals the CLI host triple - a Linux host container-building its own
    /// arch previously collapsed to `target/debug` and missed the binary
    /// (#936, round-2 finding 2).
    #[test]
    fn prebuilt_lookup_uses_the_explicit_target_dir_for_the_host_triple() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let crate_dir = dir.path().join("svc");
        std::fs::create_dir_all(&crate_dir)?;
        std::fs::write(
            crate_dir.join("Cargo.toml"),
            "[package]\nname = \"svc\"\nversion = \"0.0.0\"\nedition = \"2021\"\n",
        )?;
        let host = crate::resolve::project::host_target_triple();
        let target_dir = dir.path().join("target");
        let release = target_dir.join(&host).join("release");
        std::fs::create_dir_all(&release)?;
        std::fs::write(release.join(binary_name_with_suffix("svc")), b"bin")?;

        let found = locate_prebuilt_binary(
            &crate_dir,
            "svc",
            &target_dir,
            Some(&host),
            Profile::Release,
        )?;
        assert_eq!(found, release.join(binary_name_with_suffix("svc")));

        // The implicit `target/release` location must NOT satisfy the lookup.
        std::fs::remove_file(release.join(binary_name_with_suffix("svc")))?;
        let plain = target_dir.join("release");
        std::fs::create_dir_all(&plain)?;
        std::fs::write(plain.join(binary_name_with_suffix("svc")), b"bin")?;
        assert!(
            locate_prebuilt_binary(
                &crate_dir,
                "svc",
                &target_dir,
                Some(&host),
                Profile::Release,
            )
            .is_err(),
            "the prebuilt lookup must never collapse to the implicit target/release"
        );
        Ok(())
    }
}
