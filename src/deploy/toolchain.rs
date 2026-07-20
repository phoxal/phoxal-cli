//! Rust target and cargo-zigbuild toolchain provisioning.

use super::{CARGO_ZIGBUILD_VERSION, deploy_command};
use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
#[cfg(not(test))]
use anyhow::bail;
#[cfg(not(test))]
use phoxal_cli_core::project::tooling::make_executable;
use std::ffi::OsStr;
use std::ffi::OsString;
#[cfg(not(test))]
use std::fs;
use std::path::Path;
use std::path::PathBuf;

#[cfg(not(test))]
const ZIG_PROVISION_VERSION: &str = "0.16.0";

#[derive(Debug, Clone)]
pub(crate) struct ZigbuildToolchain {
    pub(crate) path: OsString,
    pub(crate) zig_global_cache_dir: Option<PathBuf>,
    pub(crate) zig_local_cache_dir: Option<PathBuf>,
}

#[cfg(not(test))]
pub(crate) fn ensure_zigbuild_toolchain(ui: &crate::Ui) -> Result<ZigbuildToolchain> {
    let tool_root = crate::host_paths::deploy_dir()?.join("tools/zigbuild");
    let bin_dir = tool_root.join("bin");
    fs::create_dir_all(&bin_dir)
        .with_context(|| format!("failed to create {}", bin_dir.display()))?;
    let mut search_path = path_with_cache_bin(&bin_dir, std::env::var_os("PATH").as_deref())?;

    validate_cargo_available(&search_path, &bin_dir)?;
    if validate_zig_available(&search_path, &bin_dir).is_err() {
        provision_zig(ui, &tool_root, &search_path, &bin_dir)?;
        search_path = path_with_cache_bin(&bin_dir, std::env::var_os("PATH").as_deref())?;
    }
    if !cargo_zigbuild_available(&search_path) {
        provision_cargo_zigbuild(ui, &tool_root, &search_path, &bin_dir)?;
        search_path = path_with_cache_bin(&bin_dir, std::env::var_os("PATH").as_deref())?;
    }
    validate_zigbuild_toolchain(&search_path, &bin_dir)?;
    let zig_global_cache_dir = tool_root.join("zig-cache/global");
    let zig_local_cache_dir = tool_root.join("zig-cache/local");
    fs::create_dir_all(&zig_global_cache_dir)
        .with_context(|| format!("failed to create {}", zig_global_cache_dir.display()))?;
    fs::create_dir_all(&zig_local_cache_dir)
        .with_context(|| format!("failed to create {}", zig_local_cache_dir.display()))?;
    ui.info("deploy cross-build toolchain: cargo-zigbuild + zig");
    Ok(ZigbuildToolchain {
        path: search_path,
        zig_global_cache_dir: Some(zig_global_cache_dir),
        zig_local_cache_dir: Some(zig_local_cache_dir),
    })
}

#[cfg(test)]
pub(crate) fn ensure_zigbuild_toolchain(_ui: &crate::Ui) -> Result<ZigbuildToolchain> {
    Ok(ZigbuildToolchain {
        path: std::env::var_os("PATH").unwrap_or_default(),
        zig_global_cache_dir: None,
        zig_local_cache_dir: None,
    })
}

pub(crate) fn path_with_cache_bin(cache_bin: &Path, base_path: Option<&OsStr>) -> Result<OsString> {
    let mut paths = base_path
        .into_iter()
        .flat_map(std::env::split_paths)
        .collect::<Vec<_>>();
    if !paths.iter().any(|path| path == cache_bin) {
        paths.push(cache_bin.to_path_buf());
    }
    std::env::join_paths(paths).context("failed to construct deploy toolchain PATH")
}

pub(crate) fn validate_zigbuild_toolchain(search_path: &OsStr, cache_bin: &Path) -> Result<()> {
    validate_cargo_available(search_path, cache_bin)?;
    validate_zig_available(search_path, cache_bin)?;
    if cargo_zigbuild_available(search_path) {
        Ok(())
    } else {
        Err(missing_cargo_zigbuild_error(cache_bin))
    }
}

pub(crate) fn validate_cargo_available(search_path: &OsStr, cache_bin: &Path) -> Result<()> {
    executable_on_search_path("cargo", search_path)
        .map(|_| ())
        .ok_or_else(|| missing_cargo_error(cache_bin))
}

pub(crate) fn validate_zig_available(search_path: &OsStr, cache_bin: &Path) -> Result<()> {
    let Some(zig) = executable_on_search_path("zig", search_path) else {
        return Err(missing_zig_error(cache_bin));
    };
    command_success(&zig, ["version"], search_path)
        .then_some(())
        .ok_or_else(|| missing_zig_error(cache_bin))
}

pub(crate) fn cargo_zigbuild_available(search_path: &OsStr) -> bool {
    if let Some(cargo_zigbuild) = executable_on_search_path("cargo-zigbuild", search_path)
        && command_success(&cargo_zigbuild, ["--version"], search_path)
    {
        return true;
    }
    let Some(cargo) = executable_on_search_path("cargo", search_path) else {
        return false;
    };
    command_success(&cargo, ["zigbuild", "--help"], search_path)
}

pub(crate) fn command_success<I, S>(program: &Path, args: I, search_path: &OsStr) -> bool
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    deploy_command(program.as_os_str())
        .args(args)
        .env("PATH", search_path)
        .output()
        .ok()
        .is_some_and(|output| output.status.success())
}

pub(crate) fn executable_on_search_path(name: &str, search_path: &OsStr) -> Option<PathBuf> {
    for directory in std::env::split_paths(search_path) {
        let candidate = directory.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
        #[cfg(windows)]
        {
            let candidate = directory.join(format!("{name}.exe"));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

#[cfg(not(test))]
pub(crate) fn provision_zig(
    ui: &crate::Ui,
    tool_root: &Path,
    search_path: &OsStr,
    cache_bin: &Path,
) -> Result<()> {
    let descriptor =
        zig_download_descriptor().ok_or_else(|| unprovisionable_zig_error(cache_bin, None))?;
    let zig_root = tool_root.join("zig");
    fs::create_dir_all(&zig_root)
        .with_context(|| format!("failed to create {}", zig_root.display()))?;
    let archive = zig_root.join(format!("{}.tar.xz", descriptor.archive_name));
    if !archive.is_file() {
        ui.info(format!(
            "provisioning zig {ZIG_PROVISION_VERSION} into {}",
            zig_root.display()
        ));
        let partial = archive.with_extension("partial");
        let output = deploy_command("curl")
            .args([
                "--fail",
                "--location",
                "--connect-timeout",
                "10",
                "--max-time",
                "300",
                "--output",
            ])
            .arg(&partial)
            .arg(descriptor.url)
            .env("PATH", search_path)
            .output()
            .map_err(|error| unprovisionable_zig_error(cache_bin, Some(error.to_string())))?;
        if !output.status.success() {
            return Err(unprovisionable_zig_error(
                cache_bin,
                Some(format!("curl exited with {}", output.status)),
            ));
        }
        fs::rename(&partial, &archive).with_context(|| {
            format!(
                "failed to finalize downloaded zig archive {}",
                archive.display()
            )
        })?;
    }

    let extracted = zig_root.join(descriptor.archive_name);
    let zig_binary = extracted.join("zig");
    if !zig_binary.is_file() {
        let output = deploy_command("tar")
            .arg("-xf")
            .arg(&archive)
            .arg("-C")
            .arg(&zig_root)
            .env("PATH", search_path)
            .output()
            .map_err(|error| unprovisionable_zig_error(cache_bin, Some(error.to_string())))?;
        if !output.status.success() {
            return Err(unprovisionable_zig_error(
                cache_bin,
                Some(format!("tar exited with {}", output.status)),
            ));
        }
    }
    if !zig_binary.is_file() {
        return Err(unprovisionable_zig_error(
            cache_bin,
            Some(format!(
                "archive did not contain expected binary {}",
                zig_binary.display()
            )),
        ));
    }
    let cached = cache_bin.join("zig");
    fs::copy(&zig_binary, &cached).with_context(|| {
        format!(
            "failed to stage zig from {} to {}",
            zig_binary.display(),
            cached.display()
        )
    })?;
    make_executable(&cached)?;
    Ok(())
}

#[cfg(not(test))]
pub(crate) struct ZigDownloadDescriptor {
    archive_name: &'static str,
    url: &'static str,
}

#[cfg(not(test))]
pub(crate) fn zig_download_descriptor() -> Option<ZigDownloadDescriptor> {
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    {
        return Some(ZigDownloadDescriptor {
            archive_name: "zig-aarch64-macos-0.16.0",
            url: "https://ziglang.org/download/0.16.0/zig-aarch64-macos-0.16.0.tar.xz",
        });
    }
    #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
    {
        return Some(ZigDownloadDescriptor {
            archive_name: "zig-x86_64-macos-0.16.0",
            url: "https://ziglang.org/download/0.16.0/zig-x86_64-macos-0.16.0.tar.xz",
        });
    }
    #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
    {
        return Some(ZigDownloadDescriptor {
            archive_name: "zig-aarch64-linux-0.16.0",
            url: "https://ziglang.org/download/0.16.0/zig-aarch64-linux-0.16.0.tar.xz",
        });
    }
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        return Some(ZigDownloadDescriptor {
            archive_name: "zig-x86_64-linux-0.16.0",
            url: "https://ziglang.org/download/0.16.0/zig-x86_64-linux-0.16.0.tar.xz",
        });
    }
    #[allow(unreachable_code)]
    None
}

#[cfg(not(test))]
pub(crate) fn provision_cargo_zigbuild(
    ui: &crate::Ui,
    tool_root: &Path,
    search_path: &OsStr,
    cache_bin: &Path,
) -> Result<()> {
    let cargo = executable_on_search_path("cargo", search_path)
        .ok_or_else(|| missing_cargo_error(cache_bin))?;
    ui.info(format!(
        "provisioning cargo-zigbuild {CARGO_ZIGBUILD_VERSION} into {}",
        tool_root.display()
    ));
    let output = deploy_command(cargo.as_os_str())
        .args([
            "install",
            "cargo-zigbuild",
            "--locked",
            "--version",
            CARGO_ZIGBUILD_VERSION,
            "--root",
        ])
        .arg(tool_root)
        .env("PATH", search_path)
        .output()
        .context("failed to start cargo install cargo-zigbuild")?;
    if output.status.success() {
        return Ok(());
    }
    bail!(
        "CrossBuildToolchainMissing: cargo-zigbuild is required for deploy musl cross-builds and managed provisioning failed with status {}. Fix: run `cargo install cargo-zigbuild --locked --version {CARGO_ZIGBUILD_VERSION}` with network access, or place `cargo-zigbuild` in {}.",
        output.status,
        cache_bin.display()
    )
}

pub(crate) fn missing_cargo_error(cache_bin: &Path) -> anyhow::Error {
    anyhow!(
        "CrossBuildToolchainMissing: cargo is required before deploy can run cargo-zigbuild. Fix: install Rust with rustup, then run `rustup target add aarch64-unknown-linux-musl` and `cargo install cargo-zigbuild --locked --version {CARGO_ZIGBUILD_VERSION}`. The managed cache bin is {}.",
        cache_bin.display()
    )
}

pub(crate) fn missing_zig_error(cache_bin: &Path) -> anyhow::Error {
    anyhow!(
        "CrossBuildToolchainMissing: zig is required for deploy musl cross-builds and was not found on PATH or in {}. Fix: run `brew install zig` on macOS, or install Zig from https://ziglang.org/download/ and put `zig` on PATH, then rerun `phoxal-cli deploy`.",
        cache_bin.display()
    )
}

#[cfg(not(test))]
pub(crate) fn unprovisionable_zig_error(cache_bin: &Path, detail: Option<String>) -> anyhow::Error {
    let detail = detail
        .map(|detail| format!(" Managed provisioning detail: {detail}."))
        .unwrap_or_default();
    anyhow!(
        "CrossBuildToolchainMissing: zig is required for deploy musl cross-builds and managed provisioning into {} could not complete.{detail} Fix: run `brew install zig` on macOS, or install Zig {ZIG_PROVISION_VERSION} from https://ziglang.org/download/{ZIG_PROVISION_VERSION}/ and put `zig` on PATH, then rerun `phoxal-cli deploy`.",
        cache_bin.display()
    )
}

pub(crate) fn missing_cargo_zigbuild_error(cache_bin: &Path) -> anyhow::Error {
    anyhow!(
        "CrossBuildToolchainMissing: cargo-zigbuild {CARGO_ZIGBUILD_VERSION} is required for deploy musl cross-builds and was not found on PATH or in {}. Fix: run `cargo install cargo-zigbuild --locked --version {CARGO_ZIGBUILD_VERSION}`, then rerun `phoxal-cli deploy`.",
        cache_bin.display()
    )
}
