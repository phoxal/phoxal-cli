//! Transport-independent deployment contracts and target planning.

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

/// How an official artifact reaches the robot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OfficialDelivery {
    RobotDownload,
    HostTransferFallback,
}

/// Host architecture and artifact triples used by deployment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetTriples {
    pub arch: String,
    pub official_triple: String,
    pub local_triple: String,
}

/// Robot-side download manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DownloadDescriptor {
    pub schema: String,
    pub concurrency: usize,
    pub retries: usize,
    pub artifacts: Vec<DownloadArtifact>,
}

/// One verified official artifact download.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DownloadArtifact {
    pub package: String,
    pub version: String,
    pub target: String,
    pub url: String,
    pub size: u64,
    pub sha256: String,
    pub archive_binary_name: String,
    pub install_binary_name: String,
}

/// Resolve a user-provided architecture selector into deployment triples.
pub fn target_from_selector(selector: &str) -> Result<TargetTriples> {
    match selector {
        "mender" | "rauc" => bail!("--target {selector} is reserved for future OS-update adapters"),
        "compose" | "balena" => {
            bail!("--target {selector} is not supported; deploy renders native systemd only")
        }
        "aarch64" | "arm64" | "aarch64-unknown-linux-gnu" | "aarch64-unknown-linux-musl" => {
            Ok(target_for_arch("aarch64"))
        }
        "x86_64" | "amd64" | "x86_64-unknown-linux-gnu" | "x86_64-unknown-linux-musl" => {
            Ok(target_for_arch("x86_64"))
        }
        other => bail!(
            "unsupported deploy target '{other}'; expected aarch64 or x86_64 (mender/rauc reserved)"
        ),
    }
}

/// Resolve the architecture returned by a robot's `uname -m`.
pub fn target_from_uname_arch(arch: &str) -> Result<TargetTriples> {
    match arch.trim() {
        "aarch64" | "arm64" => Ok(target_for_arch("aarch64")),
        "x86_64" | "amd64" => Ok(target_for_arch("x86_64")),
        other => {
            bail!("unsupported robot arch '{other}' from uname -m; expected aarch64 or x86_64")
        }
    }
}

/// Build official and locally cross-built triples for a normalized architecture.
#[must_use]
pub fn target_for_arch(arch: &str) -> TargetTriples {
    TargetTriples {
        arch: arch.to_string(),
        official_triple: format!("{arch}-unknown-linux-gnu"),
        local_triple: format!("{arch}-unknown-linux-musl"),
    }
}
