use std::env;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::AppContext;
use crate::unit::Unit;
use crate::unit::container::BuildPlatform;

const MACOS_CROSS_TAP: &str = "messense/macos-cross-toolchains";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DoctorStatus {
    Ok,
    Missing,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DoctorItem {
    pub label: String,
    pub status: DoctorStatus,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DoctorReport {
    pub items: Vec<DoctorItem>,
}

impl DoctorReport {
    pub fn has_missing(&self) -> bool {
        self.items
            .iter()
            .any(|item| matches!(item.status, DoctorStatus::Missing))
    }
}

#[derive(Debug, Clone)]
pub struct CheckDoctor {
    pub platform: BuildPlatform,
}

#[derive(Debug, Clone)]
pub struct FixDoctor {
    pub platform: BuildPlatform,
}

impl Unit for CheckDoctor {
    type Output = DoctorReport;

    fn name(&self) -> &'static str {
        "Doctor"
    }

    fn execute(&self, _app: &AppContext) -> Result<Self::Output> {
        collect_doctor_report(&self.platform)
    }
}

impl Unit for FixDoctor {
    type Output = DoctorReport;

    fn name(&self) -> &'static str {
        "Doctor Fix"
    }

    fn execute(&self, app: &AppContext) -> Result<Self::Output> {
        fix_doctor(&self.platform, app)?;
        collect_doctor_report(&self.platform)
    }
}

pub fn validate_rust_build_toolchain(target_triple: &str) -> Result<()> {
    validate_rust_target_installed(target_triple)?;
    validate_target_linker_available(target_triple)?;
    Ok(())
}

pub fn cargo_target_linker_env_name(target_triple: &str) -> String {
    format!(
        "CARGO_TARGET_{}_LINKER",
        target_triple.replace('-', "_").to_uppercase()
    )
}

pub fn configured_target_linker(target_triple: &str) -> Option<String> {
    env::var(cargo_target_linker_env_name(target_triple))
        .ok()
        .filter(|value| !value.trim().is_empty())
}

pub fn default_target_linker(target_triple: &str) -> Option<&'static str> {
    match target_triple {
        "aarch64-unknown-linux-gnu" => Some("aarch64-linux-gnu-gcc"),
        "x86_64-unknown-linux-gnu" => Some("x86_64-linux-gnu-gcc"),
        _ => None,
    }
}

pub fn executable_on_path(executable: &str) -> bool {
    if executable.contains(std::path::MAIN_SEPARATOR) {
        return Path::new(executable).is_file();
    }

    env::var_os("PATH")
        .map(|path| {
            env::split_paths(&path).any(|directory| {
                let candidate = directory.join(executable);
                candidate.is_file()
            })
        })
        .unwrap_or(false)
}

pub fn configured_or_default_target_linker(target_triple: &str) -> Option<String> {
    configured_target_linker(target_triple)
        .or_else(|| default_target_linker(target_triple).map(str::to_string))
}

fn collect_doctor_report(platform: &BuildPlatform) -> Result<DoctorReport> {
    let items = vec![
        binary_check("docker", "docker"),
        command_check("docker compose", "docker", &["compose", "version"]),
        binary_check("webots", "webots"),
        binary_check("rerun", "rerun"),
        rust_target_check(&platform.target_triple)?,
        target_linker_check(&platform.target_triple),
    ];

    Ok(DoctorReport { items })
}

fn binary_check(label: &str, executable: &str) -> DoctorItem {
    if executable_on_path(executable) {
        DoctorItem {
            label: label.to_string(),
            status: DoctorStatus::Ok,
            detail: format!("found `{executable}`"),
        }
    } else {
        DoctorItem {
            label: label.to_string(),
            status: DoctorStatus::Missing,
            detail: format!("missing `{executable}` on PATH"),
        }
    }
}

fn command_check(label: &str, executable: &str, args: &[&str]) -> DoctorItem {
    if !executable_on_path(executable) {
        return DoctorItem {
            label: label.to_string(),
            status: DoctorStatus::Missing,
            detail: format!("missing `{executable}` on PATH"),
        };
    }

    match Command::new(executable).args(args).output() {
        Ok(output) if output.status.success() => DoctorItem {
            label: label.to_string(),
            status: DoctorStatus::Ok,
            detail: format!("`{executable} {}` is available", args.join(" ")),
        },
        Ok(output) => DoctorItem {
            label: label.to_string(),
            status: DoctorStatus::Missing,
            detail: format!(
                "`{executable} {}` failed with status {}",
                args.join(" "),
                output.status
            ),
        },
        Err(_) => DoctorItem {
            label: label.to_string(),
            status: DoctorStatus::Missing,
            detail: format!("failed to run `{executable} {}`", args.join(" ")),
        },
    }
}

fn rust_target_check(target_triple: &str) -> Result<DoctorItem> {
    if !executable_on_path("rustup") {
        return Ok(DoctorItem {
            label: format!("rust target {target_triple}"),
            status: DoctorStatus::Missing,
            detail: "missing `rustup` on PATH".to_string(),
        });
    }

    let output = Command::new("rustup")
        .args(["target", "list", "--installed"])
        .output()
        .context("failed to query installed Rust targets with rustup")?;
    if !output.status.success() {
        bail!(
            "failed to query installed Rust targets with rustup (status {})",
            output.status
        );
    }

    let installed_targets = String::from_utf8_lossy(&output.stdout);
    let installed = installed_targets
        .lines()
        .any(|line| line.trim() == target_triple);
    Ok(DoctorItem {
        label: format!("rust target {target_triple}"),
        status: if installed {
            DoctorStatus::Ok
        } else {
            DoctorStatus::Missing
        },
        detail: if installed {
            format!("installed `{target_triple}`")
        } else {
            format!("missing `rustup target add {target_triple}`")
        },
    })
}

fn target_linker_check(target_triple: &str) -> DoctorItem {
    if let Some(configured_linker) = configured_target_linker(target_triple) {
        return DoctorItem {
            label: format!("target linker {target_triple}"),
            status: if executable_on_path(&configured_linker) {
                DoctorStatus::Ok
            } else {
                DoctorStatus::Missing
            },
            detail: if executable_on_path(&configured_linker) {
                format!(
                    "found configured linker `{}` via `{}`",
                    configured_linker,
                    cargo_target_linker_env_name(target_triple)
                )
            } else {
                format!(
                    "configured linker `{}` via `{}` was not found on PATH",
                    configured_linker,
                    cargo_target_linker_env_name(target_triple)
                )
            },
        };
    }

    let Some(default_linker) = default_target_linker(target_triple) else {
        return DoctorItem {
            label: format!("target linker {target_triple}"),
            status: DoctorStatus::Ok,
            detail: format!("no default linker check for `{target_triple}`"),
        };
    };

    DoctorItem {
        label: format!("target linker {target_triple}"),
        status: if executable_on_path(default_linker) {
            DoctorStatus::Ok
        } else {
            DoctorStatus::Missing
        },
        detail: if executable_on_path(default_linker) {
            format!("found default linker `{default_linker}`")
        } else {
            format!("missing cross linker `{default_linker}`")
        },
    }
}

fn validate_rust_target_installed(target_triple: &str) -> Result<()> {
    let item = rust_target_check(target_triple)?;
    if matches!(item.status, DoctorStatus::Ok) {
        return Ok(());
    }
    bail!("{}", item.detail)
}

fn validate_target_linker_available(target_triple: &str) -> Result<()> {
    let item = target_linker_check(target_triple);
    if matches!(item.status, DoctorStatus::Ok) {
        return Ok(());
    }

    if let Some(configured_linker) = configured_target_linker(target_triple) {
        bail!(
            "configured linker '{}' for target '{}' was not found on PATH",
            configured_linker,
            target_triple
        );
    }

    let default_linker = default_target_linker(target_triple)
        .ok_or_else(|| anyhow::anyhow!("missing target linker for '{target_triple}'"))?;
    bail!(
        "missing cross C linker '{}' for target '{}'. xtask image builds use plain `cargo build --target {}` and require the host cross toolchain to be installed; there is no fallback to native cargo builds",
        default_linker,
        target_triple,
        target_triple
    );
}

fn fix_doctor(platform: &BuildPlatform, app: &AppContext) -> Result<()> {
    ensure_rust_target(platform, app)?;
    ensure_target_linker(platform, app)?;
    Ok(())
}

fn ensure_rust_target(platform: &BuildPlatform, app: &AppContext) -> Result<()> {
    if matches!(
        rust_target_check(&platform.target_triple)?.status,
        DoctorStatus::Ok
    ) {
        return Ok(());
    }

    let title = format!("Installing Rust target {}", platform.target_triple);
    let mut command = Command::new("rustup");
    command.args(["target", "add", &platform.target_triple]);
    app.ui.step(&title, || {
        let status = app.ui.command_status(&mut command)?;
        if !status.success() {
            bail!(
                "rustup target add {} failed with status {status}",
                platform.target_triple
            );
        }
        Ok(())
    })?;
    Ok(())
}

fn ensure_target_linker(platform: &BuildPlatform, app: &AppContext) -> Result<()> {
    if matches!(
        target_linker_check(&platform.target_triple).status,
        DoctorStatus::Ok
    ) {
        return Ok(());
    }

    if configured_target_linker(&platform.target_triple).is_some() {
        bail!(
            "configured linker '{}' is missing; install it or update `{}`",
            configured_target_linker(&platform.target_triple).unwrap_or_default(),
            cargo_target_linker_env_name(&platform.target_triple)
        );
    }

    let formula = brew_formula_for_target(&platform.target_triple).ok_or_else(|| {
        anyhow::anyhow!(
            "xtask doctor fix does not know how to install a linker for '{}'",
            platform.target_triple
        )
    })?;
    if !cfg!(target_os = "macos") {
        bail!(
            "xtask doctor fix only knows how to install '{}' automatically on macOS",
            formula
        );
    }
    if !executable_on_path("brew") {
        bail!("Homebrew is required to install '{}'", formula);
    }

    let tap_title = format!("Tapping {}", MACOS_CROSS_TAP);
    let mut tap = Command::new("brew");
    tap.args(["tap", MACOS_CROSS_TAP]);
    app.ui.step(&tap_title, || {
        let tap_status = app.ui.command_status(&mut tap)?;
        if !tap_status.success() {
            bail!(
                "brew tap {} failed with status {}",
                MACOS_CROSS_TAP,
                tap_status
            );
        }
        Ok(())
    })?;

    let install_title = format!("Installing linker toolchain {}", formula);
    let mut install = Command::new("brew");
    install.args(["install", formula]);
    app.ui.step(&install_title, || {
        let install_status = app.ui.command_status(&mut install)?;
        if !install_status.success() {
            bail!(
                "brew install {} failed with status {}",
                formula,
                install_status
            );
        }
        Ok(())
    })?;

    Ok(())
}

fn brew_formula_for_target(target_triple: &str) -> Option<&str> {
    match target_triple {
        "aarch64-unknown-linux-gnu" => Some("aarch64-unknown-linux-gnu"),
        "x86_64-unknown-linux-gnu" => Some("x86_64-unknown-linux-gnu"),
        _ => None,
    }
}
