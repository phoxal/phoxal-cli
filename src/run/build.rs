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

/// Build one source participant while routing captured output through the session.
pub(crate) fn build_source_binary(
    crate_dir: &Path,
    preferred_name: &str,
    ui: &crate::Ui,
) -> Result<PathBuf> {
    let crate_dir = crate_dir.canonicalize().with_context(|| {
        format!(
            "failed to canonicalize source crate {}",
            crate_dir.display()
        )
    })?;
    let binary_name = cargo_binary_name(&crate_dir, Some(preferred_name))?;
    let package_name = cargo_package_name(&crate_dir)?;
    ui.info(format!(
        "building user participant {preferred_name} with cargo build -p {package_name} --bin {binary_name}"
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
    Ok(cargo_target_dir(&crate_dir)?
        .join("debug")
        .join(binary_name_with_suffix(&binary_name)))
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
