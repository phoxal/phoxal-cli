//! Cross-build execution and compiler-failure classification.

use super::{ZigbuildToolchain, deploy_command};
use anyhow::Context;
use anyhow::Result;
#[cfg(test)]
use anyhow::anyhow;
use anyhow::bail;
use phoxal_cli_core::project::tooling::{cargo_binary_name, cargo_package_name};
#[cfg(test)]
use serde_json::Value;
use std::path::Path;
use std::path::PathBuf;

#[cfg(not(test))]
pub(crate) fn cross_build_source_binary(
    crate_dir: &Path,
    preferred_name: &str,
    target: &str,
    toolchain: &ZigbuildToolchain,
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
    let target_dir = crate::host_paths::deploy_dir()?.join("target").join(target);
    ui.info(format!(
        "cross-building {preferred_name} for {target} with cargo zigbuild --release"
    ));
    let mut command = deploy_command("cargo");
    command
        .arg("zigbuild")
        .arg("--release")
        .arg("--target")
        .arg(target)
        .arg("--target-dir")
        .arg(&target_dir)
        .arg("-p")
        .arg(&package_name)
        .arg("--bin")
        .arg(&binary_name)
        .current_dir(&crate_dir)
        .env("PATH", &toolchain.path);
    if let Some(cache_dir) = &toolchain.zig_global_cache_dir {
        command.env("ZIG_GLOBAL_CACHE_DIR", cache_dir);
    }
    if let Some(cache_dir) = &toolchain.zig_local_cache_dir {
        command.env("ZIG_LOCAL_CACHE_DIR", cache_dir);
    }
    let output = command.output().with_context(|| {
        format!(
            "failed to start cargo zigbuild for deploy participant {preferred_name} in {}",
            crate_dir.display()
        )
    })?;
    if !output.status.success() {
        bail!(
            "{}",
            classify_zigbuild_failure(preferred_name, target, &output.stdout, &output.stderr)
        );
    }
    Ok(target_dir.join(target).join("release").join(binary_name))
}

#[cfg(test)]
pub(crate) fn cross_build_source_binary(
    crate_dir: &Path,
    preferred_name: &str,
    _target: &str,
    toolchain: &ZigbuildToolchain,
    ui: &crate::Ui,
) -> Result<PathBuf> {
    let _ = &toolchain.path;
    let _ = &toolchain.zig_global_cache_dir;
    let _ = &toolchain.zig_local_cache_dir;
    let crate_dir = crate_dir.canonicalize().with_context(|| {
        format!(
            "failed to canonicalize source crate {}",
            crate_dir.display()
        )
    })?;
    let binary_name = cargo_binary_name(&crate_dir, Some(preferred_name))?;
    let package_name = cargo_package_name(&crate_dir)?;
    ui.info(format!(
        "test-building deploy participant {preferred_name} with cargo build --release"
    ));
    let status = deploy_command("cargo")
        .arg("build")
        .arg("--release")
        .arg("-p")
        .arg(&package_name)
        .arg("--bin")
        .arg(&binary_name)
        .current_dir(&crate_dir)
        .status()
        .with_context(|| {
            format!(
                "failed to start cargo build for deploy participant {preferred_name} in {}",
                crate_dir.display()
            )
        })?;
    if !status.success() {
        bail!(
            "cargo build failed for deploy participant {preferred_name} in {} with status {status}",
            crate_dir.display()
        );
    }
    Ok(cargo_target_dir(&crate_dir)?
        .join("release")
        .join(binary_name_with_suffix(&binary_name)))
}

#[cfg(test)]
pub(crate) fn cargo_target_dir(crate_dir: &Path) -> Result<PathBuf> {
    let output = deploy_command("cargo")
        .args(["metadata", "--format-version", "1", "--no-deps"])
        .current_dir(crate_dir)
        .output()
        .context("failed to run `cargo`")?;
    if !output.status.success() {
        bail!(
            "`cargo` failed with status {}\nstdout:\n{}\nstderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let output = String::from_utf8(output.stdout).context("`cargo` wrote non-UTF8 stdout")?;
    let json: Value = serde_json::from_str(&output).context("cargo metadata was not JSON")?;
    json.get("target_directory")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .ok_or_else(|| anyhow!("cargo metadata did not include target_directory"))
}

#[cfg(test)]
pub(crate) fn binary_name_with_suffix(binary_name: &str) -> String {
    if cfg!(windows) {
        format!("{binary_name}.exe")
    } else {
        binary_name.to_string()
    }
}

pub(crate) fn classify_zigbuild_failure(
    preferred_name: &str,
    target: &str,
    stdout: &[u8],
    stderr: &[u8],
) -> String {
    let output = format!(
        "{}\n{}",
        String::from_utf8_lossy(stdout),
        String::from_utf8_lossy(stderr)
    );
    if let Some(crate_name) = native_sysroot_failure_crate(&output) {
        return format!(
            "CrossBuildUnsupported: deploy participant {preferred_name} cannot be cross-built for {target} because crate '{crate_name}' needs target-native system headers/libs that are not in the zig sysroot. Fix: provide a pinned target sysroot for cargo-zigbuild, publish a CI-built native artifact, or remove/feature-gate that dependency."
        );
    }
    format!(
        "CrossBuildFailed: cargo zigbuild failed for deploy participant {preferred_name} on {target}. Run `cargo zigbuild --release --target {target}` in the participant crate for the full compiler output."
    )
}

pub(crate) fn native_sysroot_failure_crate(output: &str) -> Option<String> {
    let lower = output.to_ascii_lowercase();
    let looks_sysroot_related = [
        "pkg-config has not been configured to support cross-compilation",
        "pkg_config_path",
        "could not find system library",
        "could not find directory of openssl installation",
        "failed to find tool",
        "no such file or directory",
        "fatal error:",
        "file not found",
        "cannot find -l",
        "library not found for -l",
    ]
    .iter()
    .any(|needle| lower.contains(needle));
    if !looks_sysroot_related {
        return None;
    }
    failed_build_crate(output).or_else(|| {
        if lower.contains("openssl") {
            Some("openssl-sys".to_string())
        } else if lower.contains("opencv") {
            Some("opencv".to_string())
        } else {
            None
        }
    })
}

pub(crate) fn failed_build_crate(output: &str) -> Option<String> {
    for line in output.lines() {
        if let Some(rest) = line
            .split("failed to run custom build command for `")
            .nth(1)
        {
            return rest.split('`').next().and_then(crate_name_from_package_id);
        }
        if let Some(rest) = line.split("required by crate `").nth(1) {
            return rest.split('`').next().and_then(crate_name_from_package_id);
        }
    }
    None
}

pub(crate) fn crate_name_from_package_id(package_id: &str) -> Option<String> {
    package_id
        .split_whitespace()
        .next()
        .filter(|name| !name.is_empty())
        .map(str::to_string)
}
