//! Deployment command options and transport-independent data models.

use super::{
    DownloadArtifact, DownloadDescriptor, OfficialDelivery, TargetTriples, parse_deploy_host,
    read_password_from_tty, sudo_password_from_env,
};
use anyhow::Context;
use anyhow::Result;
use clap::Args;
use phoxal_cli_core::project::catalog::ArtifactKind;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;
use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;
use tempfile::TempDir;
use zeroize::Zeroize;
use zeroize::ZeroizeOnDrop;

#[derive(Debug, Args)]
pub struct Deploy {
    #[arg(
        value_name = "USER@HOST",
        value_parser = parse_deploy_host,
        help = "Robot SSH destination."
    )]
    pub host: Option<String>,
    #[arg(
        long,
        help = "Render, validate, and cross-build without mutating the host (a host, if given, is probed read-only)."
    )]
    pub dry_run: bool,
    #[arg(
        long,
        value_name = "ARCH",
        help = "Override the dry-run target arch (aarch64 or x86_64); by default a dry-run probes the host's arch. Required for a hostless dry-run. mender/rauc reserved; compose/balena unsupported."
    )]
    pub target: Option<String>,
    #[arg(
        long = "env",
        value_name = "ENV",
        help = "Apply a robot.<env>.yaml overlay before deploying (repeatable)."
    )]
    pub env: Vec<String>,
    #[arg(
        long,
        default_value_t = 30,
        help = "Health readiness deadline after restart, in seconds."
    )]
    pub health_timeout_sec: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DeployOptions {
    pub host: Option<String>,
    pub dry_run: bool,
    pub target: Option<String>,
    pub overlays: Vec<String>,
    pub catalog_source: Option<String>,
    pub health_timeout: Duration,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DeployReport {
    pub mode: &'static str,
    pub target_arch: String,
    pub official_target_triple: String,
    pub local_target_triple: String,
    pub payload_root: PathBuf,
    pub install_plan: InstallPlan,
    pub rendered_units: BTreeMap<String, String>,
    pub env_files: BTreeMap<String, String>,
    pub release_json: Value,
    pub delivery: Option<OfficialDelivery>,
    pub health: Option<HealthReport>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstallPlan {
    pub helper_path: String,
    pub sudoers_path: String,
    pub scoped_delete: Vec<String>,
    pub direct_writes: Vec<String>,
    pub identity_files: Vec<IdentityInstallPlan>,
    pub units: Vec<String>,
    pub stale_units_to_remove: Vec<String>,
    pub lifecycle: Vec<String>,
    pub health_deadline_seconds: u64,
    pub watchdog_sec: u64,
    pub missing_official_artifacts: Vec<String>,
    pub release_generation: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdentityInstallPlan {
    pub local_path: PathBuf,
    pub remote_path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealthReport {
    pub units: Vec<HealthUnitReport>,
}

impl HealthReport {
    #[must_use]
    pub fn is_ok(&self) -> bool {
        self.units.iter().all(|unit| unit.ready)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealthUnitReport {
    pub unit: String,
    pub participant: Option<String>,
    pub ready: bool,
    pub active_state: String,
    pub sub_state: String,
    pub journal_excerpt: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RemoteProbe {
    pub arch: String,
    pub bootstrap_required: bool,
    /// The account `ssh <host>` lands as; bootstrap enrolls this user into
    /// the `phoxal-deploy` group, since it is the one that runs
    /// `sudo phoxal-systemd-helper`.
    pub remote_user: String,
    /// `sudo -n true` succeeded: any root work can run fully non-interactively.
    pub sudo_noninteractive: bool,
    /// The group-model grant is fully in place and works for this user: the
    /// helper is installed and executable, this user is a member of the
    /// `phoxal-deploy` group, and a non-interactive
    /// `sudo phoxal-systemd-helper` call is authorized through the static
    /// sudoers fragment. Blanket passwordless sudo (`sudo_noninteractive`)
    /// does NOT set this by itself: a device with blanket sudo but no
    /// `phoxal-deploy` membership (e.g. bootstrapped under the old per-user
    /// sudoers model) must still trigger the bootstrap repair path so it
    /// converges to the group model.
    pub helper_grant: bool,
    /// The installed helper differs from this build's expected script.
    pub helper_stale: bool,
}

impl RemoteProbe {
    /// Root work is needed when the host was never bootstrapped, or when the
    /// deploying user is not covered by the group-model grant (e.g. a new
    /// operator who has never deployed to this host, or a device still on
    /// the old per-user sudoers model), or when the helper script itself is
    /// stale. Re-running the bootstrap script is the repair: it rewrites the
    /// helper, (re)writes the static `phoxal-deploy` group sudoers fragment,
    /// and enrolls this user into the `phoxal-deploy` group, all
    /// idempotently.
    pub(crate) fn root_work_required(&self) -> bool {
        self.bootstrap_required || !self.helper_grant || self.helper_stale
    }
}

pub(crate) struct SudoPassword {
    pub(crate) bytes: Vec<u8>,
}

impl SudoPassword {
    pub(crate) fn new(bytes: Vec<u8>) -> Self {
        Self { bytes }
    }

    pub(crate) fn push(&mut self, byte: u8) {
        self.bytes.push(byte);
    }

    pub(crate) fn write_with_newline(&self, writer: &mut impl Write) -> Result<()> {
        writer
            .write_all(&self.bytes)
            .context("failed to write sudo password to child stdin")?;
        writer
            .write_all(b"\n")
            .context("failed to write sudo password newline to child stdin")
    }
}

impl Zeroize for SudoPassword {
    fn zeroize(&mut self) {
        self.bytes.zeroize();
    }
}

impl ZeroizeOnDrop for SudoPassword {}

impl Drop for SudoPassword {
    fn drop(&mut self) {
        self.zeroize();
    }
}

pub(crate) trait SudoPasswordSource {
    fn password_from_env(&mut self) -> Option<SudoPassword>;
    fn read_password(&mut self, prompt: &str) -> Result<SudoPassword>;
}

pub(crate) struct LocalSudoPasswordSource;

impl SudoPasswordSource for LocalSudoPasswordSource {
    fn password_from_env(&mut self) -> Option<SudoPassword> {
        sudo_password_from_env()
    }

    fn read_password(&mut self, prompt: &str) -> Result<SudoPassword> {
        read_password_from_tty(prompt)
    }
}

pub(crate) trait DeployTransport {
    fn probe(&mut self) -> Result<RemoteProbe>;
    fn validate_sudo_password(&mut self, password: &SudoPassword) -> Result<bool>;
    fn bootstrap(
        &mut self,
        helper: &BootstrapScripts,
        sudo_password: Option<&SudoPassword>,
    ) -> Result<()>;
    fn list_installed_units(&mut self) -> Result<Vec<String>>;
    fn github_release_reachable(&mut self, url: &str) -> Result<bool>;
    fn prepare_host_transfer_fallback(
        &mut self,
        payload: &mut RenderedPayload,
        ui: &crate::Ui,
    ) -> Result<()>;
    fn sync_payload(&mut self, payload: &RenderedPayload) -> Result<()>;
    fn download_official_artifacts(
        &mut self,
        generation: &str,
        artifacts: &[DownloadArtifact],
    ) -> Result<()>;
    fn install_units(&mut self, payload: &RenderedPayload, stale_units: &[String]) -> Result<()>;
    fn activate_release(&mut self, generation: &str) -> Result<()>;
    fn rollback_release(&mut self) -> Result<()>;
    fn finalize_units(&mut self, stale_units: &[String]) -> Result<()>;
    fn restart(&mut self) -> Result<()>;
    fn health_report(&mut self, units: &[String], deadline: Duration) -> Result<HealthReport>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BootstrapScripts {
    pub helper_script: String,
    pub sudoers_fragment: String,
    /// The deploying user to enroll into the `phoxal-deploy` group.
    /// Validated against a conservative username charset before it reaches
    /// here, since the bootstrap script interpolates it directly into shell.
    pub remote_user: String,
}

#[derive(Debug)]
pub(crate) struct RenderedPayload {
    pub root: TempDir,
    pub target: TargetTriples,
    pub install_plan: InstallPlan,
    pub rendered_units: BTreeMap<String, String>,
    pub env_files: BTreeMap<String, String>,
    pub release_json: Value,
    pub download_descriptor: DownloadDescriptor,
    pub(crate) official_plans: BTreeMap<String, OfficialArtifactPlan>,
    pub delivery: Option<OfficialDelivery>,
    pub unit_names: Vec<String>,
    pub bootstrap: BootstrapScripts,
}

#[derive(Debug, Clone)]
pub(crate) struct SourceBuildArtifact {
    pub(crate) artifact_id: String,
    pub(crate) kind: ArtifactKind,
    pub(crate) source: Value,
    pub(crate) sha256: String,
    pub(crate) payload_path: PathBuf,
}

#[derive(Debug, Clone)]
pub(crate) struct OfficialArtifactPlan {
    pub(crate) artifact_id: String,
    pub(crate) kind: ArtifactKind,
    pub(crate) version: String,
    pub(crate) sha256: String,
    pub(crate) url: String,
    pub(crate) size: u64,
    pub(crate) target: String,
    pub(crate) archive_binary_name: String,
    pub(crate) install_binary_name: String,
    pub(crate) source_path: Option<PathBuf>,
    pub(crate) missing_label: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct ReleaseRecord {
    pub(crate) schema: String,
    pub(crate) created_at_utc: String,
    pub(crate) artifacts: Vec<ReleaseArtifact>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct ReleaseArtifact {
    pub(crate) id: String,
    pub(crate) kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) version: Option<String>,
    pub(crate) source: Value,
    pub(crate) sha256: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) target: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) url: Option<String>,
}
