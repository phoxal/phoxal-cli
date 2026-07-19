use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::fd::{AsRawFd, RawFd};
#[cfg(unix)]
use std::os::unix::ffi::OsStringExt;
use std::path::{Component as PathComponent, Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use clap::Args;
use phoxal::model::robot::v0::{ConnectionConfig, Robot};
use phoxal::participant::launch::env;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tempfile::TempDir;
use zeroize::{Zeroize, ZeroizeOnDrop};

use crate::AppContext;
use crate::catalog::ArtifactKind;
use crate::commands::check::{
    CheckGraphContext, SourceParticipant, SourceParticipantKind, build_emit_apis_from_source,
    check_artifact_refs_from_resolved, extract_emit_apis_from_staged_runtime,
    extract_emit_apis_from_staged_tool, fetch_emit_apis_from_tool, run_check_with_context,
    source_participants_from_resolved,
};
use crate::component_driver::component_driver_crate_dir;
use crate::launch_env::{EncodedParticipantEnv, encode_participant_env};
use crate::launch_plan::{
    CheckedRobotLaunchInput, DEFAULT_ROUTER_CONNECT, LaunchMode, LaunchPlan, ParticipantExecution,
    ParticipantLaunchRecord, PlanContext, SITE_INFRASTRUCTURE_ROUTER, SITE_TOOL_JOYPAD, SiteLaunch,
    build_launch_plan,
};
use crate::resolver::{
    ResolveOptions, ResolvedComponentSource, ResolvedPlatformRuntime, ResolvedRobot, ResolvedTool,
    discover_robot_yaml, load_robot_with_extras, resolve,
};
use crate::supervisor::{START_LIMIT_BURST, START_LIMIT_INTERVAL};
use crate::utils::{cargo_binary_name, hash_tree, make_executable};

const OPT_ROOT: &str = "/opt/phoxal";
const ACTIVE_ROOT: &str = "/opt/phoxal/active";
const OPT_BIN: &str = "/opt/phoxal/active/bin";
const OPT_ENV: &str = "/opt/phoxal/active/env";
const RELEASES_ROOT: &str = "/opt/phoxal/releases";
const SYSTEMD_DIR: &str = "/etc/systemd/system";
const IDENTITY_DIR: &str = "/var/lib/phoxal/identity";
const HELPER_PATH: &str = "/usr/local/sbin/phoxal-systemd-helper";
const SUDOERS_PATH: &str = "/etc/sudoers.d/phoxal-deploy";
const PAYLOAD_STAGING_PREFIX: &str = "/tmp/phoxal-payload-";
const RELEASE_SCHEMA: &str = "phoxal.release/v0";
const DOWNLOAD_DESCRIPTOR_SCHEMA: &str = "phoxal.deploy-download/v0";
const DOWNLOAD_CONCURRENCY: usize = 4;
const DOWNLOAD_RETRIES: usize = 3;
const SUDO_PASSWORD_ENV: &str = "PHOXAL_SUDO_PASSWORD";
const COMPONENT_FILE: &str = "component.yaml";
const STRUCTURE_FILE: &str = "structure.urdf";
const SIMULATION_FILE: &str = "simulation.yaml";
const MESHES_DIR: &str = "meshes";
const WATCHDOG_SEC: u64 = 10;
const CARGO_ZIGBUILD_VERSION: &str = "0.23.0";
#[cfg(not(test))]
const ZIG_PROVISION_VERSION: &str = "0.16.0";

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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OfficialDelivery {
    RobotDownload,
    HostTransferFallback,
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
    fn root_work_required(&self) -> bool {
        self.bootstrap_required || !self.helper_grant || self.helper_stale
    }
}

pub(crate) struct SudoPassword {
    bytes: Vec<u8>,
}

impl SudoPassword {
    fn new(bytes: Vec<u8>) -> Self {
        Self { bytes }
    }

    fn push(&mut self, byte: u8) {
        self.bytes.push(byte);
    }

    fn write_with_newline(&self, writer: &mut impl Write) -> Result<()> {
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

trait SudoPasswordSource {
    fn password_from_env(&mut self) -> Option<SudoPassword>;
    fn read_password(&mut self, prompt: &str) -> Result<SudoPassword>;
}

struct LocalSudoPasswordSource;

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
    official_plans: BTreeMap<String, OfficialArtifactPlan>,
    pub delivery: Option<OfficialDelivery>,
    pub unit_names: Vec<String>,
    pub bootstrap: BootstrapScripts,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TargetTriples {
    pub arch: String,
    pub official_triple: String,
    pub local_triple: String,
}

#[derive(Debug, Clone)]
struct SourceBuildArtifact {
    artifact_id: String,
    kind: ArtifactKind,
    source: Value,
    sha256: String,
    payload_path: PathBuf,
}

#[derive(Debug, Clone)]
struct OfficialArtifactPlan {
    artifact_id: String,
    kind: ArtifactKind,
    version: String,
    sha256: String,
    url: String,
    size: u64,
    target: String,
    archive_binary_name: String,
    install_binary_name: String,
    source_path: Option<PathBuf>,
    missing_label: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DownloadDescriptor {
    pub schema: String,
    pub concurrency: usize,
    pub retries: usize,
    pub artifacts: Vec<DownloadArtifact>,
}

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

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct ReleaseRecord {
    schema: String,
    created_at_utc: String,
    artifacts: Vec<ReleaseArtifact>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct ReleaseArtifact {
    id: String,
    kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    version: Option<String>,
    source: Value,
    sha256: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    target: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    url: Option<String>,
}

impl Deploy {
    pub async fn run(&self, app: &AppContext) -> Result<()> {
        let options = DeployOptions {
            host: self.host.clone(),
            dry_run: self.dry_run,
            target: self.target.clone(),
            overlays: self.env.clone(),
            catalog_source: app.catalog_source.clone(),
            health_timeout: Duration::from_secs(self.health_timeout_sec),
        };
        let project_root = app.project.root().to_path_buf();
        let ui = app.ui;
        let result = tokio::task::spawn_blocking(move || run(&project_root, options, &ui))
            .await
            .context("deploy worker failed")??;
        eprintln!(
            "warning: v0 is pre-stable: artifacts built at different times may not interoperate"
        );
        report(result)
    }
}

pub(crate) fn run(
    project_start: &Path,
    options: DeployOptions,
    ui: &crate::Ui,
) -> Result<DeployReport> {
    validate_deploy_options(&options)?;
    if options.dry_run {
        // A dry-run against a host probes that host's arch (and reachability,
        // remote user) read-only - no mutation - so it validates against the
        // real machine instead of a hand-specified triple. `--target` overrides
        // the probe, and is required only for a hostless render (CI / offline).
        let (target, remote_user) = match options.host.as_deref() {
            Some(host) => {
                let mut transport = SshTransport::new(host.to_string(), *ui);
                let probe = transport.probe().context("failed to probe deploy host")?;
                let target = match options.target.as_deref() {
                    Some(selector) => target_from_selector(selector)?,
                    None => target_from_uname_arch(&probe.arch)?,
                };
                (target, probe.remote_user)
            }
            None => {
                let target = target_from_selector(
                    options
                        .target
                        .as_deref()
                        .context("--dry-run without a host requires --target <arch>")?,
                )?;
                (target, DRY_RUN_REMOTE_USER.to_string())
            }
        };
        let payload = prepare_deploy(project_start, &options, target, false, &remote_user, ui)?;
        return Ok(report_from_payload("dry-run", payload, None));
    }

    let host = options
        .host
        .as_deref()
        .context("deploy requires <user@host> unless --dry-run is set")?;
    let mut transport = SshTransport::new(host.to_string(), *ui);
    deploy_with_transport(
        project_start,
        &options,
        &mut transport,
        local_tty_available(),
        ui,
    )
}

pub(crate) fn deploy_with_transport<T: DeployTransport>(
    project_start: &Path,
    options: &DeployOptions,
    transport: &mut T,
    local_tty_available: bool,
    ui: &crate::Ui,
) -> Result<DeployReport> {
    let mut sudo_passwords = LocalSudoPasswordSource;
    deploy_with_transport_with_sudo(
        project_start,
        options,
        transport,
        local_tty_available,
        &mut sudo_passwords,
        ui,
    )
}

fn deploy_with_transport_with_sudo<T, S>(
    project_start: &Path,
    options: &DeployOptions,
    transport: &mut T,
    local_tty_available: bool,
    sudo_passwords: &mut S,
    ui: &crate::Ui,
) -> Result<DeployReport>
where
    T: DeployTransport,
    S: SudoPasswordSource + ?Sized,
{
    validate_deploy_options(options)?;
    let probe = transport.probe().context("failed to probe deploy host")?;
    let target = target_from_uname_arch(&probe.arch)?;
    let host = options
        .host
        .as_deref()
        .context("deploy requires <user@host> unless --dry-run is set")?;
    let sudo_password =
        ensure_sudo_will_succeed(host, &probe, local_tty_available, sudo_passwords, transport)?;
    let mut payload = prepare_deploy(project_start, options, target, true, &probe.remote_user, ui)?;

    if probe.root_work_required() {
        transport
            .bootstrap(&payload.bootstrap, sudo_password.as_ref())
            .context("failed to bootstrap remote phoxal install")?;
    }
    drop(sudo_password);
    let installed = transport
        .list_installed_units()
        .context("failed to list installed phoxal units")?;
    let stale = stale_units(&installed, &payload.unit_names);
    payload.install_plan.stale_units_to_remove = stale.clone();
    validate_download_descriptor(&payload.download_descriptor)?;

    let reachability_url = payload
        .download_descriptor
        .artifacts
        .first()
        .map(|artifact| artifact.url.as_str())
        .filter(|url| !url.is_empty())
        .context("resolved deploy has no published official release URL")?;
    let delivery = if transport
        .github_release_reachable(reachability_url)
        .context("failed to preflight GitHub release reachability from the robot")?
    {
        OfficialDelivery::RobotDownload
    } else {
        transport
            .prepare_host_transfer_fallback(&mut payload, ui)
            .context("failed to prepare host-transfer fallback artifacts")?;
        OfficialDelivery::HostTransferFallback
    };
    payload.delivery = Some(delivery);

    transport
        .sync_payload(&payload)
        .context("failed to sync phoxal payload")?;
    if delivery == OfficialDelivery::RobotDownload {
        transport
            .download_official_artifacts(
                &payload.install_plan.release_generation,
                &payload.download_descriptor.artifacts,
            )
            .context("robot failed to download official release artifacts")?;
    }
    transport
        .install_units(&payload, &stale)
        .context("failed to install systemd units")?;
    transport
        .activate_release(&payload.install_plan.release_generation)
        .context("failed to activate transactional release")?;
    let health = match restart_and_check(transport, &payload.unit_names, options.health_timeout) {
        Ok(health) => health,
        Err(deploy_error) => {
            let rollback = transport
                .rollback_release()
                .and_then(|()| transport.restart());
            return match rollback {
                Ok(()) => Err(anyhow!(
                    "{deploy_error:#}\nrelease rolled back to the previous generation"
                )),
                Err(rollback_error) => Err(anyhow!(
                    "{deploy_error:#}\nrelease activation failed and rollback also failed: {rollback_error:#}"
                )),
            };
        }
    };
    transport
        .finalize_units(&stale)
        .context("failed to finalize systemd units after healthy activation")?;
    Ok(report_from_payload("deploy", payload, Some(health)))
}

fn restart_and_check<T: DeployTransport>(
    transport: &mut T,
    units: &[String],
    deadline: Duration,
) -> Result<HealthReport> {
    transport
        .restart()
        .context("failed to restart phoxal.target")?;
    let health = transport
        .health_report(units, deadline)
        .context("failed to collect deploy health")?;
    if !health.is_ok() {
        bail!("{}", format_health_failure(&health));
    }
    Ok(health)
}

/// Fail before cross-building/packaging/rsyncing anything if sudo on the
/// target will never succeed. Decision table:
///
/// 1. `sudo -n true` works (blanket NOPASSWD or a cached credential):
///    proceed - bootstrap or grant repair can run non-interactively.
/// 2. No blanket sudo, but the group-model grant is fully in place for this
///    user (`helper_grant`: helper installed, user enrolled in
///    `phoxal-deploy`, sudoers fragment authorizes the call), and the helper
///    hash matches this build: proceed - the steady-state deploy needs no
///    root work; every `run_helper` call goes through the fragment.
/// 3. Root work is required (first bootstrap, a stale/missing grant, or a
///    stale helper that the bootstrap script repairs) and `PHOXAL_SUDO_PASSWORD`
///    is set or local `/dev/tty` is available: validate a password now, then
///    proceed and feed it to bootstrap over child stdin.
/// 4. Root work is required and there is no password env var and no local
///    `/dev/tty`: fail now, before doing any work, with all remedies.
fn ensure_sudo_will_succeed<T, S>(
    host: &str,
    probe: &RemoteProbe,
    local_tty_available: bool,
    sudo_passwords: &mut S,
    transport: &mut T,
) -> Result<Option<SudoPassword>>
where
    T: DeployTransport,
    S: SudoPasswordSource + ?Sized,
{
    if probe.sudo_noninteractive {
        return Ok(None);
    }
    if !probe.root_work_required() {
        return Ok(None);
    }
    if let Some(password) = sudo_passwords.password_from_env() {
        if transport
            .validate_sudo_password(&password)
            .with_context(|| format!("failed to validate sudo password on {host}"))?
        {
            return Ok(Some(password));
        }
        bail!(
            "DeploySudoPasswordRejected: {SUDO_PASSWORD_ENV} did not validate for {user} on {host}.",
            user = probe.remote_user,
        );
    }
    if local_tty_available {
        let prompt = sudo_password_prompt(&probe.remote_user, host);
        for _ in 0..2 {
            let password = sudo_passwords.read_password(&prompt).with_context(|| {
                format!(
                    "failed to read sudo password for {user} on {host}",
                    user = probe.remote_user
                )
            })?;
            if transport
                .validate_sudo_password(&password)
                .with_context(|| format!("failed to validate sudo password on {host}"))?
            {
                return Ok(Some(password));
            }
        }
        bail!(
            "DeploySudoPasswordRejected: sudo password validation failed for {user} on {host} after 2 attempts.",
            user = probe.remote_user,
        );
    }
    let root_work = if probe.bootstrap_required {
        "needs root once (first deploy: install /opt/phoxal, the phoxal-systemd-helper, and the phoxal-deploy group sudoers grant)"
    } else if probe.helper_stale {
        "needs root once (repair: the installed phoxal-systemd-helper is stale for this phoxal-cli build, so the deploy must rewrite the helper and the phoxal-deploy group sudoers grant)"
    } else {
        "needs root once (repair: this user is not covered by the phoxal-deploy group grant, so the deploy must install the phoxal-deploy group sudoers grant and add this user to the group)"
    };
    bail!(
        "DeploySudoRequiresPassword: {host} {root_work} and sudo is not passwordless for {user}. Fix: rerun `phoxal-cli deploy` interactively (from a real TTY so phoxal-cli can read /dev/tty), pre-authorize {user} on {host} with a NOPASSWD sudoers entry, or for automation set {SUDO_PASSWORD_ENV} for this command (NOPASSWD or an interactive run is preferred).",
        user = probe.remote_user,
    )
}

fn sudo_password_prompt(user: &str, host: &str) -> String {
    format!("[sudo] password for {user} on {host}:")
}

#[cfg(unix)]
fn sudo_password_from_env() -> Option<SudoPassword> {
    std::env::var_os(SUDO_PASSWORD_ENV).map(|password| SudoPassword::new(password.into_vec()))
}

#[cfg(not(unix))]
fn sudo_password_from_env() -> Option<SudoPassword> {
    std::env::var_os(SUDO_PASSWORD_ENV)
        .map(|password| SudoPassword::new(password.to_string_lossy().into_owned().into_bytes()))
}

fn local_tty_available() -> bool {
    open_tty().is_ok()
}

#[cfg(unix)]
fn open_tty() -> std::io::Result<fs::File> {
    OpenOptions::new().read(true).write(true).open("/dev/tty")
}

#[cfg(not(unix))]
fn open_tty() -> std::io::Result<fs::File> {
    Err(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        "/dev/tty is not available on this platform",
    ))
}

fn read_password_from_tty(prompt: &str) -> Result<SudoPassword> {
    let mut tty = open_tty().context("failed to open /dev/tty for sudo password prompt")?;
    tty.write_all(prompt.as_bytes())
        .context("failed to write sudo password prompt to /dev/tty")?;
    tty.flush()
        .context("failed to flush sudo password prompt to /dev/tty")?;
    let mut password = SudoPassword::new(Vec::new());
    {
        let _echo_guard = TtyEchoGuard::disable(&tty).context("failed to disable /dev/tty echo")?;
        loop {
            let mut byte = [0_u8; 1];
            let read = tty
                .read(&mut byte)
                .context("failed to read sudo password from /dev/tty")?;
            if read == 0 {
                bail!("failed to read sudo password from /dev/tty: EOF");
            }
            match byte[0] {
                b'\n' | b'\r' => break,
                value => password.push(value),
            }
        }
    }
    tty.write_all(b"\n")
        .context("failed to finish sudo password prompt on /dev/tty")?;
    Ok(password)
}

#[cfg(unix)]
struct TtyEchoGuard {
    fd: RawFd,
    original: libc::termios,
}

#[cfg(unix)]
impl TtyEchoGuard {
    fn disable(tty: &fs::File) -> Result<Self> {
        let fd = tty.as_raw_fd();
        let mut original = std::mem::MaybeUninit::<libc::termios>::uninit();
        if unsafe { libc::tcgetattr(fd, original.as_mut_ptr()) } != 0 {
            return Err(std::io::Error::last_os_error()).context("tcgetattr failed");
        }
        let original = unsafe { original.assume_init() };
        let mut no_echo = original;
        no_echo.c_lflag &= !(libc::ECHO as libc::tcflag_t);
        if unsafe { libc::tcsetattr(fd, libc::TCSAFLUSH, &no_echo) } != 0 {
            return Err(std::io::Error::last_os_error()).context("tcsetattr failed");
        }
        Ok(Self { fd, original })
    }
}

#[cfg(unix)]
impl Drop for TtyEchoGuard {
    fn drop(&mut self) {
        let _ = unsafe { libc::tcsetattr(self.fd, libc::TCSAFLUSH, &self.original) };
    }
}

#[cfg(not(unix))]
struct TtyEchoGuard;

#[cfg(not(unix))]
impl TtyEchoGuard {
    fn disable(_tty: &fs::File) -> Result<Self> {
        bail!("/dev/tty password prompting is only supported on Unix platforms")
    }
}

fn deploy_command(program: impl AsRef<OsStr>) -> Command {
    let mut command = Command::new(program);
    command.env_remove(SUDO_PASSWORD_ENV);
    command
}

/// Each remote command must create a fresh SSH login session. In particular,
/// the first helper call after bootstrap must not reuse a ControlMaster whose
/// server-side process still has the supplementary groups from before
/// `usermod -aG phoxal-deploy` ran.
fn deploy_ssh_command() -> Command {
    let mut command = deploy_command("ssh");
    command.args([
        "-o",
        "ControlMaster=no",
        "-o",
        "ControlPath=none",
        "-o",
        "ControlPersist=no",
    ]);
    command
}

// The prompt must be a single non-empty token: these argv vectors travel over
// `ssh <host> <args...>`, which flattens them into one shell line - an empty
// `-p ""` argument vanishes and `-p` then swallows the next token as the
// prompt (turning the script path into the command). The prompt itself only
// goes to the remote stderr; the password always arrives via stdin (-S).
const SUDO_STDIN_PROMPT: &str = "phoxal-sudo-password:";

fn sudo_validate_args() -> [&'static str; 5] {
    ["sudo", "-S", "-p", SUDO_STDIN_PROMPT, "-v"]
}

fn sudo_bootstrap_args(remote_path: &str) -> Vec<&str> {
    vec!["sudo", "-S", "-p", SUDO_STDIN_PROMPT, "sh", remote_path]
}

fn validate_deploy_options(options: &DeployOptions) -> Result<()> {
    if options.dry_run {
        if options.host.is_none() && options.target.is_none() {
            bail!(
                "--dry-run needs either <user@host> (its arch is probed read-only) or --target <arch> for a hostless render"
            );
        }
        if options
            .host
            .as_deref()
            .is_some_and(|host| host.trim().is_empty() || host.chars().any(char::is_whitespace))
        {
            bail!("deploy host must be a non-empty SSH destination without whitespace");
        }
    } else {
        let host = options
            .host
            .as_deref()
            .context("deploy requires <user@host> unless --dry-run is set")?;
        if host.trim().is_empty() || host.chars().any(char::is_whitespace) {
            bail!("deploy host must be a non-empty SSH destination without whitespace");
        }
        if let Some(target) = options.target.as_deref() {
            match target {
                "mender" | "rauc" => {
                    bail!("--target {target} is reserved for future OS-update adapters")
                }
                "compose" | "balena" => {
                    bail!("--target {target} is not supported; deploy renders native systemd only")
                }
                _ => bail!(
                    "live deploy probes the robot arch; --target is only valid with --dry-run"
                ),
            }
        }
    }
    Ok(())
}

fn parse_deploy_host(value: &str) -> Result<String, String> {
    match value {
        "build" | "push" => Err(
            "deploy has one verb; use `deploy <user@host>` or `deploy --dry-run --target <arch>`"
                .to_string(),
        ),
        value if value.trim().is_empty() || value.chars().any(char::is_whitespace) => {
            Err("deploy host must be a non-empty SSH destination without whitespace".to_string())
        }
        value => Ok(value.to_string()),
    }
}

fn target_from_selector(selector: &str) -> Result<TargetTriples> {
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

fn target_from_uname_arch(arch: &str) -> Result<TargetTriples> {
    match arch.trim() {
        "aarch64" | "arm64" => Ok(target_for_arch("aarch64")),
        "x86_64" | "amd64" => Ok(target_for_arch("x86_64")),
        other => {
            bail!("unsupported robot arch '{other}' from uname -m; expected aarch64 or x86_64")
        }
    }
}

fn target_for_arch(arch: &str) -> TargetTriples {
    TargetTriples {
        arch: arch.to_string(),
        official_triple: format!("{arch}-unknown-linux-gnu"),
        local_triple: format!("{arch}-unknown-linux-musl"),
    }
}

/// Placeholder deploy-group enrollee for `--dry-run`, which renders no host
/// and so never probes a real remote user. The rendered fragment is
/// inspectable but is never installed anywhere, since dry-run never contacts
/// a host.
const DRY_RUN_REMOTE_USER: &str = "<deploy-user>";

fn prepare_deploy(
    project_start: &Path,
    options: &DeployOptions,
    target: TargetTriples,
    _require_official_binaries: bool,
    remote_user: &str,
    ui: &crate::Ui,
) -> Result<RenderedPayload> {
    let robot_path = discover_robot_yaml(project_start)
        .with_context(|| format!("failed to find robot.yaml from {}", project_start.display()))?;
    let project_root = robot_path
        .parent()
        .context("robot.yaml did not have a parent directory")?;
    let loaded = if options.overlays.is_empty() {
        load_robot_with_extras(&robot_path)?
    } else {
        crate::resolver::load_robot_with_extras_and_overlays(&robot_path, &options.overlays)?
    };
    let catalog = crate::commands::load_catalog_for_robot_from_source(
        options.catalog_source.clone(),
        project_root,
        loaded.robot.artifacts.channel,
        &loaded.extras,
        ui.interactive(),
    )?;
    let mut resolved = resolve(
        &loaded.robot,
        project_root,
        catalog.as_ref(),
        ResolveOptions {
            refresh_channel_head: false,
            emit_update_notice: true,
            resolve_source_commits: true,
            resolve_component_asset_commits: false,
            official_target_triple: Some(target.official_triple.clone()),
            tool_target_triple: Some(target.official_triple.clone()),
            interactive: ui.interactive(),
        },
    )?;
    if let Some(catalog) = catalog.as_ref() {
        hydrate_catalog_blobs(&mut resolved, catalog)?;
    }
    if !options.dry_run {
        let mut host_resolved = resolve(
            &loaded.robot,
            project_root,
            catalog.as_ref(),
            ResolveOptions {
                refresh_channel_head: false,
                emit_update_notice: false,
                resolve_source_commits: false,
                resolve_component_asset_commits: false,
                official_target_triple: Some(crate::resolver::host_target_triple()),
                tool_target_triple: Some(crate::resolver::host_target_triple()),
                interactive: ui.interactive(),
            },
        )?;
        let catalog = catalog
            .as_ref()
            .context("deploy requires the pinned catalog to recover immutable release URLs")?;
        hydrate_catalog_blobs(&mut host_resolved, catalog)?;
        ensure_cross_target_release_match(&resolved, &host_resolved)?;
        let descriptors = crate::native_artifacts::descriptors_for(&host_resolved, false, true)?;
        crate::native_artifacts::prepare_descriptors_with_preflight(&descriptors, Some(ui))?;
        prepare_deploy_after_host_staging(
            project_root,
            options,
            target,
            remote_user,
            ui,
            &loaded,
            robot_path.clone(),
            resolved,
            host_resolved,
        )
    } else {
        prepare_deploy_after_host_staging(
            project_root,
            options,
            target,
            remote_user,
            ui,
            &loaded,
            robot_path.clone(),
            resolved.clone(),
            resolved,
        )
    }
}

fn hydrate_catalog_blobs(
    resolved: &mut ResolvedRobot,
    catalog: &crate::catalog::Catalog,
) -> Result<()> {
    fn hydrate_runtime(
        runtime: &mut ResolvedPlatformRuntime,
        catalog: &crate::catalog::Catalog,
    ) -> Result<()> {
        if runtime.path_override.is_some() || runtime.version == "source" {
            return Ok(());
        }
        let artifact = catalog
            .artifacts
            .iter()
            .find(|artifact| {
                artifact.package == runtime.package && artifact.version == runtime.version
            })
            .with_context(|| {
                format!(
                    "pinned catalog is missing {} {}",
                    runtime.package, runtime.version
                )
            })?;
        let blob = if runtime.kind == ArtifactKind::ComponentAssets {
            artifact.assets.as_ref()
        } else {
            runtime
                .target
                .as_ref()
                .and_then(|target| artifact.targets.get(target))
        };
        let Some(blob) = blob else {
            runtime.published = false;
            return Ok(());
        };
        runtime.sha256 = Some(blob.sha256.clone());
        runtime.url = Some(blob.url.clone());
        runtime.size = Some(blob.size);
        runtime.published = true;
        runtime.published_triples = artifact.targets.keys().cloned().collect();
        Ok(())
    }

    for runtime in &mut resolved.platform_runtimes {
        hydrate_runtime(runtime, catalog)?;
    }
    for component in &mut resolved.components {
        if let Some(runtime) = component
            .assets
            .as_mut()
            .and_then(|assets| assets.catalog_runtime.as_mut())
        {
            hydrate_runtime(runtime, catalog)?;
        }
        if let Some(runtime) = component
            .driver
            .as_mut()
            .and_then(|driver| driver.catalog_runtime.as_mut())
        {
            hydrate_runtime(runtime, catalog)?;
        }
    }
    for tool in &mut resolved.tools {
        if tool.path_override.is_some() || tool.resolved == "source" {
            continue;
        }
        let artifact = catalog
            .artifacts
            .iter()
            .find(|artifact| artifact.package == tool.package && artifact.version == tool.resolved)
            .with_context(|| {
                format!(
                    "pinned catalog is missing {} {}",
                    tool.package, tool.resolved
                )
            })?;
        let Some(blob) = artifact.targets.get(&tool.target) else {
            tool.published = false;
            continue;
        };
        tool.asset = blob.url.clone();
        tool.sha256 = blob.sha256.clone();
        tool.url = Some(blob.url.clone());
        tool.size = Some(blob.size);
        tool.published = true;
    }
    Ok(())
}

fn ensure_cross_target_release_match(robot: &ResolvedRobot, host: &ResolvedRobot) -> Result<()> {
    let robot_versions = official_runtimes_including_component_drivers(robot)
        .into_iter()
        .map(|runtime| (runtime.package.as_str(), runtime.version.as_str()))
        .chain(
            robot
                .tools
                .iter()
                .map(|tool| (tool.package.as_str(), tool.resolved.as_str())),
        )
        .collect::<BTreeMap<_, _>>();
    let host_versions = official_runtimes_including_component_drivers(host)
        .into_iter()
        .map(|runtime| (runtime.package.as_str(), runtime.version.as_str()))
        .chain(
            host.tools
                .iter()
                .map(|tool| (tool.package.as_str(), tool.resolved.as_str())),
        )
        .collect::<BTreeMap<_, _>>();
    for (package, robot_version) in robot_versions {
        let host_version = host_versions
            .get(package)
            .with_context(|| format!("host-target coherence evidence is missing {package}"))?;
        if *host_version != robot_version {
            bail!(
                "CrossTargetReleaseMismatch: robot target resolved {package} {robot_version}, but host-target coherence evidence resolved {host_version}; run `phoxal update` for both target scopes before deploy"
            );
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn prepare_deploy_after_host_staging(
    project_root: &Path,
    options: &DeployOptions,
    target: TargetTriples,
    remote_user: &str,
    ui: &crate::Ui,
    loaded: &crate::resolver::LoadedRobot,
    robot_path: PathBuf,
    resolved: ResolvedRobot,
    host_resolved: ResolvedRobot,
) -> Result<RenderedPayload> {
    let all_source_participants =
        source_participants_from_resolved(project_root, &resolved, component_driver_crate_dir)?;
    let checked_source_participants = all_source_participants
        .iter()
        .filter(|participant| {
            !matches!(
                participant.kind,
                SourceParticipantKind::Tool | SourceParticipantKind::Simulator
            )
        })
        .cloned()
        .collect::<Vec<_>>();
    let coherence_source_participants = checked_source_participants.clone();
    ensure_no_native_c_source_dependencies(&checked_source_participants)?;
    let platform_refs = check_artifact_refs_from_resolved(&host_resolved)
        .into_iter()
        .filter(|artifact| artifact.kind != ArtifactKind::Tool)
        .collect::<Vec<_>>();
    let tool_participants = Vec::new();
    let mut official_by_ref = host_resolved
        .platform_runtimes
        .iter()
        .map(|runtime| (runtime.artifact_ref().to_string(), runtime))
        .collect::<BTreeMap<_, _>>();
    official_by_ref.extend(crate::commands::check::component_driver_runtimes_by_ref(
        &host_resolved,
    ));
    let tools_by_ref = host_resolved
        .tools
        .iter()
        .map(|tool| (tool.asset.clone(), tool))
        .collect::<BTreeMap<_, _>>();
    let outcome = run_check_with_context(
        &platform_refs,
        &tool_participants,
        &coherence_source_participants,
        CheckGraphContext {
            manifest_extras: &loaded.extras,
        },
        |artifact_ref| {
            if let Some(runtime) = official_by_ref.get(artifact_ref) {
                return extract_emit_apis_from_staged_runtime(runtime);
            }
            if let Some(tool) = tools_by_ref.get(artifact_ref) {
                return extract_emit_apis_from_staged_tool(tool);
            }
            Err(anyhow!(
                "resolved official artifact {artifact_ref} is not in the catalog"
            ))
        },
        fetch_emit_apis_from_tool,
        build_emit_apis_from_source,
    )?;
    crate::commands::check::ensure_check_outcome_ok(&resolved.channel.to_string(), &outcome)?;

    let plan = build_launch_plan(
        LaunchMode::Deploy,
        &[CheckedRobotLaunchInput {
            project_root,
            resolved: &resolved,
            manifest_extras: &loaded.extras,
            checked_participants: &outcome.checked_participants,
            substitutions: &[],
            source_participants: &checked_source_participants,
        }],
    )?;
    let coherence_graph = crate::commands::check::robot_contract_surfaces(
        &resolved.robot.robot.id,
        &outcome.contract_surfaces,
    );
    let coherence = crate::commands::check::coherence_for_launch_plan(&plan, &[coherence_graph])?;
    crate::commands::check::enforce_coherence(
        crate::commands::check::CoherenceVerb::Deploy,
        &coherence,
    )?;

    let project_root = project_root.to_path_buf();
    let ctx = PlanContext {
        robot_path,
        project_root,
        resolved,
        source_participants: all_source_participants,
    };

    render_payload(RenderPayloadInput {
        robot: &loaded.robot,
        ctx: &ctx,
        plan: &plan,
        target,
        health_timeout: options.health_timeout,
        remote_user,
        ui,
    })
}

struct RenderPayloadInput<'a> {
    robot: &'a Robot,
    ctx: &'a PlanContext,
    plan: &'a LaunchPlan,
    target: TargetTriples,
    health_timeout: Duration,
    remote_user: &'a str,
    ui: &'a crate::Ui,
}

fn render_payload(input: RenderPayloadInput<'_>) -> Result<RenderedPayload> {
    let RenderPayloadInput {
        robot,
        ctx,
        plan,
        target,
        health_timeout,
        remote_user,
        ui,
    } = input;
    let project_root = ctx.project_root.as_path();
    let resolved = &ctx.resolved;
    let source_participants = ctx.source_participants.as_slice();
    // The hostless `--dry-run` sentinel is never installed anywhere (see its
    // doc comment), so it is exempt from the real-username charset check.
    if remote_user != DRY_RUN_REMOTE_USER {
        validate_remote_username(remote_user)?;
    }
    let root = tempfile::tempdir().context("failed to create deploy payload directory")?;
    create_payload_dirs(root.path())?;

    let bootstrap = BootstrapScripts {
        helper_script: helper_script(),
        sudoers_fragment: sudoers_fragment(),
        remote_user: remote_user.to_string(),
    };

    let source_builds = stage_source_artifacts(
        project_root,
        root.path(),
        resolved,
        source_participants,
        plan,
        &target,
        ui,
    )?;
    let official_plans = stage_official_artifacts(root.path(), resolved, plan, false)?;

    let identity_files = Vec::new();
    let mut env_files = BTreeMap::new();
    render_env_files(root.path(), plan, &mut env_files)?;

    let mut rendered_units = BTreeMap::new();
    let unit_names = render_units(
        root.path(),
        resolved,
        plan,
        &source_builds,
        &official_plans,
        &mut rendered_units,
    )?;

    write_robot_yaml(root.path(), robot)?;
    let metadata_files = stage_payload_metadata(project_root, root.path(), robot, resolved)?;

    let download_descriptor = download_descriptor(&official_plans);
    let descriptor_text = serde_json::to_string_pretty(&download_descriptor)
        .context("failed to encode deploy download descriptor")?;
    write_text(
        &payload_opt(root.path()).join("download-descriptor.json"),
        &(descriptor_text + "\n"),
    )?;

    let release = release_record(resolved, plan, &source_builds, &official_plans)?;
    let release_json_text =
        serde_json::to_string_pretty(&release).context("failed to encode release record")?;
    write_text(
        &payload_opt(root.path()).join("phoxal-release.json"),
        &(release_json_text.clone() + "\n"),
    )?;
    let release_json = serde_json::from_str::<Value>(&release_json_text)?;

    let missing_official_artifacts = official_plans
        .values()
        .filter_map(|artifact| artifact.missing_label.clone())
        .collect::<Vec<_>>();
    let mut direct_writes = vec![
        format!("{ACTIVE_ROOT}/robot.yaml"),
        format!("{ACTIVE_ROOT}/phoxal-release.json"),
    ];
    direct_writes.extend(metadata_files);
    let release_generation = release_generation(&release_json_text)?;
    let install_plan = InstallPlan {
        helper_path: HELPER_PATH.to_string(),
        sudoers_path: SUDOERS_PATH.to_string(),
        scoped_delete: Vec::new(),
        direct_writes,
        identity_files,
        units: unit_names.clone(),
        stale_units_to_remove: Vec::new(),
        lifecycle: vec![
            "daemon-reload".to_string(),
            "enable phoxal.target and generated phoxal-* services".to_string(),
            "restart phoxal.target".to_string(),
            "health report".to_string(),
        ],
        health_deadline_seconds: health_timeout.as_secs(),
        watchdog_sec: WATCHDOG_SEC,
        missing_official_artifacts,
        release_generation,
    };

    let install_plan_text =
        serde_json::to_string_pretty(&install_plan).context("failed to encode install plan")?;
    write_text(
        root.path().join("install-plan.json").as_path(),
        &(install_plan_text + "\n"),
    )?;

    Ok(RenderedPayload {
        root,
        target,
        install_plan,
        rendered_units,
        env_files,
        release_json,
        download_descriptor,
        official_plans,
        delivery: None,
        unit_names,
        bootstrap,
    })
}

fn create_payload_dirs(root: &Path) -> Result<()> {
    for path in [payload_bin(root), payload_env(root), payload_systemd(root)] {
        fs::create_dir_all(&path)
            .with_context(|| format!("failed to create {}", path.display()))?;
    }
    Ok(())
}

fn download_descriptor(
    official_plans: &BTreeMap<String, OfficialArtifactPlan>,
) -> DownloadDescriptor {
    DownloadDescriptor {
        schema: DOWNLOAD_DESCRIPTOR_SCHEMA.to_string(),
        concurrency: DOWNLOAD_CONCURRENCY,
        retries: DOWNLOAD_RETRIES,
        artifacts: official_plans
            .values()
            .map(|artifact| DownloadArtifact {
                package: artifact.artifact_id.clone(),
                version: artifact.version.clone(),
                target: artifact.target.clone(),
                url: artifact.url.clone(),
                size: artifact.size,
                sha256: artifact.sha256.clone(),
                archive_binary_name: artifact.archive_binary_name.clone(),
                install_binary_name: artifact.install_binary_name.clone(),
            })
            .collect(),
    }
}

fn validate_download_descriptor(descriptor: &DownloadDescriptor) -> Result<()> {
    if descriptor.artifacts.is_empty() {
        bail!("resolved deploy has no official artifacts to download");
    }
    for artifact in &descriptor.artifacts {
        if !artifact.url.starts_with("https://")
            || artifact.size == 0
            || !phoxal::catalog::is_sha256(&artifact.sha256)
        {
            bail!(
                "NativePending: official artifact {} {} has no complete immutable blob for {}; run `phoxal update` and verify the catalog publishes this robot target",
                artifact.package,
                artifact.version,
                artifact.target
            );
        }
    }
    Ok(())
}

fn stage_official_fallback(payload: &mut RenderedPayload, ui: &crate::Ui) -> Result<()> {
    let descriptors = payload
        .official_plans
        .values()
        .map(
            |artifact| crate::native_artifacts::NativeArtifactDescriptor {
                package_id: artifact.artifact_id.clone(),
                kind: artifact.kind,
                name: artifact.artifact_id.clone(),
                version: artifact.version.clone(),
                url: artifact.url.clone(),
                sha256: artifact.sha256.clone(),
                size: artifact.size,
                binary_name: artifact.archive_binary_name.clone(),
                target: Some(artifact.target.clone()),
            },
        )
        .collect::<Vec<_>>();
    crate::native_artifacts::prepare_descriptors_with_preflight(&descriptors, Some(ui))?;
    let _lock = crate::native_artifacts::ArtifactStoreLock::shared()?;
    for (descriptor, artifact) in descriptors.iter().zip(payload.official_plans.values_mut()) {
        let source = crate::native_artifacts::artifact_binary_path(descriptor)?;
        let dest = payload_bin(payload.root.path()).join(&artifact.install_binary_name);
        fs::copy(&source, &dest).with_context(|| {
            format!(
                "failed to stage fallback artifact {} from {}",
                artifact.artifact_id,
                source.display()
            )
        })?;
        make_executable(&dest)?;
        artifact.source_path = Some(source);
    }
    Ok(())
}

fn run_bounded<T, F>(items: &[T], concurrency: usize, operation: F) -> Result<()>
where
    T: Sync,
    F: Fn(&T) -> Result<()> + Sync,
{
    if concurrency == 0 {
        bail!("download concurrency must be greater than zero");
    }
    for batch in items.chunks(concurrency) {
        std::thread::scope(|scope| -> Result<()> {
            let handles = batch
                .iter()
                .map(|item| scope.spawn(|| operation(item)))
                .collect::<Vec<_>>();
            for handle in handles {
                handle
                    .join()
                    .map_err(|_| anyhow!("robot download worker panicked"))??;
            }
            Ok(())
        })?;
    }
    Ok(())
}

fn release_generation(release_json: &str) -> Result<String> {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before Unix epoch")?
        .as_nanos();
    let mut digest = Sha256::new();
    digest.update(release_json.as_bytes());
    digest.update(nonce.to_le_bytes());
    Ok(hex::encode(digest.finalize())[..16].to_string())
}

fn payload_opt(root: &Path) -> PathBuf {
    root.join("opt/phoxal")
}

fn payload_bin(root: &Path) -> PathBuf {
    root.join("opt/phoxal/bin")
}

fn payload_env(root: &Path) -> PathBuf {
    root.join("opt/phoxal/env")
}

fn payload_systemd(root: &Path) -> PathBuf {
    root.join("opt/phoxal/systemd")
}

fn stage_source_artifacts(
    project_root: &Path,
    root: &Path,
    resolved: &ResolvedRobot,
    source_participants: &[SourceParticipant],
    plan: &LaunchPlan,
    target: &TargetTriples,
    ui: &crate::Ui,
) -> Result<BTreeMap<String, SourceBuildArtifact>> {
    let needed = needed_source_artifact_ids(plan, source_participants);
    let mut artifacts = BTreeMap::new();

    for participant in source_participants {
        let artifact_id = source_artifact_id(participant);
        if !needed.contains(&artifact_id) {
            continue;
        }
        if artifacts.contains_key(&artifact_id) {
            continue;
        }
        let artifact = build_source_artifact(
            project_root,
            root,
            resolved,
            participant,
            &artifact_id,
            target,
            ui,
        )?;
        artifacts.insert(artifact_id, artifact);
    }

    Ok(artifacts)
}

fn needed_source_artifact_ids(
    plan: &LaunchPlan,
    source_participants: &[SourceParticipant],
) -> BTreeSet<String> {
    let source_by_participant = source_participants
        .iter()
        .map(|participant| (participant.name.as_str(), participant))
        .collect::<BTreeMap<_, _>>();
    plan.robots
        .iter()
        .flat_map(|robot| &robot.participants)
        .filter_map(|participant| {
            source_by_participant.get(participant.launch.participant_id.as_str())
        })
        .map(|participant| source_artifact_id(participant))
        .collect()
}

fn source_artifact_id(participant: &SourceParticipant) -> String {
    match participant.kind {
        SourceParticipantKind::UserService => participant.expected_artifact_id.clone(),
        SourceParticipantKind::OfficialService => {
            format!("service-{}", participant.expected_artifact_id)
        }
        SourceParticipantKind::ComponentDriver => {
            format!("driver-{}", participant.expected_artifact_id)
        }
        SourceParticipantKind::Tool => participant.name.clone(),
        SourceParticipantKind::Simulator => {
            format!("simulator-{}", participant.expected_artifact_id)
        }
    }
}

fn build_source_artifact(
    project_root: &Path,
    root: &Path,
    resolved: &ResolvedRobot,
    participant: &SourceParticipant,
    artifact_id: &str,
    target: &TargetTriples,
    ui: &crate::Ui,
) -> Result<SourceBuildArtifact> {
    if let Some(native_dep) = native_sysroot_dependency(&participant.crate_dir)? {
        return Err(cross_build_unsupported_error(
            participant.kind_label(),
            &participant.name,
            &native_dep,
        ));
    }
    ensure_rust_target(&target.local_triple, ui)?;
    let toolchain = ensure_zigbuild_toolchain(ui)?;
    let actual_binary = cross_build_source_binary(
        &participant.crate_dir,
        artifact_id,
        &target.local_triple,
        &toolchain,
        ui,
    )
    .with_context(|| {
        format!(
            "failed to cross-build {} {} for {}",
            participant.kind_label(),
            participant.name,
            target.local_triple
        )
    })?;
    let dest = payload_bin(root).join(artifact_id);
    fs::copy(&actual_binary, &dest).with_context(|| {
        format!(
            "failed to stage source binary {} to {}",
            actual_binary.display(),
            dest.display()
        )
    })?;
    make_executable(&dest)?;
    let sha256 = sha256_file(&dest)?;
    let source = source_record(project_root, resolved, participant)?;
    Ok(SourceBuildArtifact {
        artifact_id: artifact_id.to_string(),
        kind: source_kind(participant.kind),
        source,
        sha256,
        payload_path: dest,
    })
}

fn source_kind(kind: SourceParticipantKind) -> ArtifactKind {
    use crate::participant_kind::ParticipantKind;
    match kind.shared_kind() {
        ParticipantKind::Service => ArtifactKind::Service,
        ParticipantKind::Driver => ArtifactKind::ComponentDriver,
        ParticipantKind::Tool => ArtifactKind::Tool,
        ParticipantKind::Simulator => ArtifactKind::Simulator,
    }
}

fn source_record(
    project_root: &Path,
    resolved: &ResolvedRobot,
    participant: &SourceParticipant,
) -> Result<Value> {
    if participant.kind == SourceParticipantKind::ComponentDriver
        && let Some(component) = resolved
            .components
            .iter()
            .find(|component| component.instance == participant.name)
        && let Some(driver) = &component.driver
    {
        return match &driver.source {
            ResolvedComponentSource::Git { git, rev, .. } => {
                Ok(serde_json::json!({ "git": git, "rev": rev }))
            }
            ResolvedComponentSource::Path { path } => {
                let full = crate::utils::resolve_project_path(project_root, path);
                Ok(serde_json::json!({
                    "path": path.display().to_string(),
                    "tree": format!("sha256:{}", hash_tree(&full)?)
                }))
            }
            ResolvedComponentSource::Catalog => {
                Ok(serde_json::json!({ "package": driver.package }))
            }
        };
    }

    let display_path = path_relative_to(project_root, &participant.crate_dir);
    Ok(serde_json::json!({
        "path": display_path.display().to_string(),
        "tree": format!("sha256:{}", hash_tree(&participant.crate_dir)?)
    }))
}

fn path_relative_to(root: &Path, path: &Path) -> PathBuf {
    pathdiff::diff_paths(path, root).unwrap_or_else(|| path.to_path_buf())
}

fn native_sysroot_dependency(crate_dir: &Path) -> Result<Option<String>> {
    let manifest_path = crate_dir.join("Cargo.toml");
    let contents = fs::read_to_string(&manifest_path)
        .with_context(|| format!("failed to read {}", manifest_path.display()))?;
    let manifest = toml::from_str::<toml::Value>(&contents)
        .with_context(|| format!("failed to parse {}", manifest_path.display()))?;
    let mut names = Vec::new();
    collect_dependency_names(&manifest, &mut names);
    names.sort();
    names.dedup();
    Ok(names.into_iter().find(|name| {
        name == "opencv"
            || name == "libudev"
            || name == "v4l"
            || name == "v4l2"
            || name == "libusb"
            || name == "realsense-rust"
    }))
}

fn ensure_no_native_c_source_dependencies(participants: &[SourceParticipant]) -> Result<()> {
    for participant in participants {
        if let Some(native_dep) = native_sysroot_dependency(&participant.crate_dir)? {
            return Err(cross_build_unsupported_error(
                participant.kind_label(),
                &participant.name,
                &native_dep,
            ));
        }
    }
    Ok(())
}

fn cross_build_unsupported_error(kind: &str, name: &str, native_dep: &str) -> anyhow::Error {
    anyhow!(
        "CrossBuildUnsupported: {kind} {name} depends on native sysroot crate '{native_dep}', which cargo-zigbuild cannot make portable by itself. Fix: provide the target-native headers/libs in a pinned sysroot, publish a CI-built native artifact, or remove/feature-gate the dependency."
    )
}

fn collect_dependency_names(value: &toml::Value, names: &mut Vec<String>) {
    let Some(table) = value.as_table() else {
        return;
    };
    for (key, value) in table {
        if matches!(
            key.as_str(),
            "dependencies" | "dev-dependencies" | "build-dependencies"
        ) && let Some(deps) = value.as_table()
        {
            names.extend(deps.keys().cloned());
        }
        collect_dependency_names(value, names);
    }
}

#[cfg(not(test))]
fn ensure_rust_target(target: &str, ui: &crate::Ui) -> Result<()> {
    let output = deploy_command("rustup")
        .args(["target", "list", "--installed"])
        .output()
        .context("CrossBuildUnsupported: rustup is required to manage deploy cross targets")?;
    if !output.status.success() {
        bail!(
            "CrossBuildUnsupported: rustup is required to manage deploy cross targets and `rustup target list --installed` failed with status {}.",
            output.status
        );
    }
    let installed = String::from_utf8(output.stdout)
        .context("CrossBuildUnsupported: rustup wrote non-UTF8 stdout")?;
    if installed.lines().any(|line| line.trim() == target) {
        return Ok(());
    }
    ui.info(format!("provisioning Rust target {target} with rustup"));
    let status = deploy_command("rustup")
        .args(["target", "add", target])
        .status()
        .context("failed to start rustup target add")?;
    if status.success() {
        return Ok(());
    }
    bail!(
        "CrossBuildTargetMissing: rustup could not install target {target} (status {status}). Fix: run `rustup target add {target}` with network access, then rerun `phoxal-cli deploy`."
    )
}

#[cfg(test)]
fn ensure_rust_target(_target: &str, _ui: &crate::Ui) -> Result<()> {
    Ok(())
}

#[derive(Debug, Clone)]
struct ZigbuildToolchain {
    path: OsString,
    zig_global_cache_dir: Option<PathBuf>,
    zig_local_cache_dir: Option<PathBuf>,
}

#[cfg(not(test))]
fn ensure_zigbuild_toolchain(ui: &crate::Ui) -> Result<ZigbuildToolchain> {
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
fn ensure_zigbuild_toolchain(_ui: &crate::Ui) -> Result<ZigbuildToolchain> {
    Ok(ZigbuildToolchain {
        path: std::env::var_os("PATH").unwrap_or_default(),
        zig_global_cache_dir: None,
        zig_local_cache_dir: None,
    })
}

fn path_with_cache_bin(cache_bin: &Path, base_path: Option<&OsStr>) -> Result<OsString> {
    let mut paths = base_path
        .into_iter()
        .flat_map(std::env::split_paths)
        .collect::<Vec<_>>();
    if !paths.iter().any(|path| path == cache_bin) {
        paths.push(cache_bin.to_path_buf());
    }
    std::env::join_paths(paths).context("failed to construct deploy toolchain PATH")
}

fn validate_zigbuild_toolchain(search_path: &OsStr, cache_bin: &Path) -> Result<()> {
    validate_cargo_available(search_path, cache_bin)?;
    validate_zig_available(search_path, cache_bin)?;
    if cargo_zigbuild_available(search_path) {
        Ok(())
    } else {
        Err(missing_cargo_zigbuild_error(cache_bin))
    }
}

fn validate_cargo_available(search_path: &OsStr, cache_bin: &Path) -> Result<()> {
    executable_on_search_path("cargo", search_path)
        .map(|_| ())
        .ok_or_else(|| missing_cargo_error(cache_bin))
}

fn validate_zig_available(search_path: &OsStr, cache_bin: &Path) -> Result<()> {
    let Some(zig) = executable_on_search_path("zig", search_path) else {
        return Err(missing_zig_error(cache_bin));
    };
    command_success(&zig, ["version"], search_path)
        .then_some(())
        .ok_or_else(|| missing_zig_error(cache_bin))
}

fn cargo_zigbuild_available(search_path: &OsStr) -> bool {
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

fn command_success<I, S>(program: &Path, args: I, search_path: &OsStr) -> bool
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

fn executable_on_search_path(name: &str, search_path: &OsStr) -> Option<PathBuf> {
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
fn provision_zig(
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
struct ZigDownloadDescriptor {
    archive_name: &'static str,
    url: &'static str,
}

#[cfg(not(test))]
fn zig_download_descriptor() -> Option<ZigDownloadDescriptor> {
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
fn provision_cargo_zigbuild(
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

fn missing_cargo_error(cache_bin: &Path) -> anyhow::Error {
    anyhow!(
        "CrossBuildToolchainMissing: cargo is required before deploy can run cargo-zigbuild. Fix: install Rust with rustup, then run `rustup target add aarch64-unknown-linux-musl` and `cargo install cargo-zigbuild --locked --version {CARGO_ZIGBUILD_VERSION}`. The managed cache bin is {}.",
        cache_bin.display()
    )
}

fn missing_zig_error(cache_bin: &Path) -> anyhow::Error {
    anyhow!(
        "CrossBuildToolchainMissing: zig is required for deploy musl cross-builds and was not found on PATH or in {}. Fix: run `brew install zig` on macOS, or install Zig from https://ziglang.org/download/ and put `zig` on PATH, then rerun `phoxal-cli deploy`.",
        cache_bin.display()
    )
}

#[cfg(not(test))]
fn unprovisionable_zig_error(cache_bin: &Path, detail: Option<String>) -> anyhow::Error {
    let detail = detail
        .map(|detail| format!(" Managed provisioning detail: {detail}."))
        .unwrap_or_default();
    anyhow!(
        "CrossBuildToolchainMissing: zig is required for deploy musl cross-builds and managed provisioning into {} could not complete.{detail} Fix: run `brew install zig` on macOS, or install Zig {ZIG_PROVISION_VERSION} from https://ziglang.org/download/{ZIG_PROVISION_VERSION}/ and put `zig` on PATH, then rerun `phoxal-cli deploy`.",
        cache_bin.display()
    )
}

fn missing_cargo_zigbuild_error(cache_bin: &Path) -> anyhow::Error {
    anyhow!(
        "CrossBuildToolchainMissing: cargo-zigbuild {CARGO_ZIGBUILD_VERSION} is required for deploy musl cross-builds and was not found on PATH or in {}. Fix: run `cargo install cargo-zigbuild --locked --version {CARGO_ZIGBUILD_VERSION}`, then rerun `phoxal-cli deploy`.",
        cache_bin.display()
    )
}

#[cfg(not(test))]
fn cross_build_source_binary(
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
fn cross_build_source_binary(
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
    ui.info(format!(
        "test-building deploy participant {preferred_name} with cargo build --release"
    ));
    let status = deploy_command("cargo")
        .arg("build")
        .arg("--release")
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
fn cargo_target_dir(crate_dir: &Path) -> Result<PathBuf> {
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
fn binary_name_with_suffix(binary_name: &str) -> String {
    if cfg!(windows) {
        format!("{binary_name}.exe")
    } else {
        binary_name.to_string()
    }
}

fn classify_zigbuild_failure(
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

fn native_sysroot_failure_crate(output: &str) -> Option<String> {
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

fn failed_build_crate(output: &str) -> Option<String> {
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

fn crate_name_from_package_id(package_id: &str) -> Option<String> {
    package_id
        .split_whitespace()
        .next()
        .filter(|name| !name.is_empty())
        .map(str::to_string)
}

/// Every resolved runtime deploy stages like a service: the platform runtimes
/// AND every Catalog-sourced component driver's `catalog_runtime` (docs #21 -
/// a driver projects onto the identical `ResolvedPlatformRuntime` shape). One
/// component id may back several instances sharing the same driver package;
/// this yields each distinct one once.
fn official_runtimes_including_component_drivers(
    resolved: &ResolvedRobot,
) -> Vec<&ResolvedPlatformRuntime> {
    let mut seen = BTreeSet::new();
    let mut runtimes = Vec::new();
    for runtime in &resolved.platform_runtimes {
        if seen.insert(runtime.package.clone()) {
            runtimes.push(runtime);
        }
    }
    for driver in resolved
        .components
        .iter()
        .filter_map(|component| component.driver.as_ref())
        .filter(|driver| matches!(driver.source, ResolvedComponentSource::Catalog))
    {
        if let Some(runtime) = &driver.catalog_runtime
            && seen.insert(runtime.package.clone())
        {
            runtimes.push(runtime);
        }
    }
    runtimes
}

/// Find a resolved runtime (service, simulator, or Catalog-sourced component
/// driver) by its participant/launch `artifact_id` - a driver's is the
/// component id (`runtime.name`), matching how a service's is its own name.
fn official_runtime_by_artifact_id<'a>(
    resolved: &'a ResolvedRobot,
    artifact_id: &str,
) -> Option<&'a ResolvedPlatformRuntime> {
    official_runtimes_including_component_drivers(resolved)
        .into_iter()
        .find(|runtime| runtime.name == artifact_id)
}

fn stage_official_artifacts(
    root: &Path,
    resolved: &ResolvedRobot,
    plan: &LaunchPlan,
    require_binaries: bool,
) -> Result<BTreeMap<String, OfficialArtifactPlan>> {
    let mut artifacts = BTreeMap::new();
    for runtime in official_runtimes_including_component_drivers(resolved) {
        if !plan.robots.iter().any(|robot| {
            robot.participants.iter().any(|participant| {
                participant.artifact_id == runtime.name
                    && matches!(
                        participant.execution,
                        ParticipantExecution::OfficialArtifact { .. }
                    )
            })
        }) {
            continue;
        }
        let plan = official_runtime_plan(root, runtime, require_binaries)?;
        if require_binaries && plan.source_path.is_none() {
            bail!(
                "NativePending: official artifact {} is not vendored for {}; run `phoxal update`, set PHOXAL_ARTIFACT_{}_PATH, or set PHOXAL_ARTIFACT_DIR",
                runtime.package,
                resolved.target,
                env_key(&runtime.package)
            );
        }
        artifacts.insert(runtime.package.clone(), plan);
    }
    // Every standard site tool (`tool-router`, and - CLI-UX Phase 4 -
    // `tool-joypad`/`tool-telemetry`) stages the same way: locate its
    // resolved tool entry and, unless it is a local path-pin override, plan
    // its official binary. Generalized from a router-only block now that
    // deploy ships every standard site tool, not just the router.
    for site in &plan.site {
        let tool = resolved
            .tools
            .iter()
            .find(|tool| tool.name == site.id)
            .with_context(|| format!("resolved deploy graph is missing site tool {}", site.id))?;
        if tool.path_override.is_some() {
            continue;
        }
        let plan = official_tool_plan(root, tool)?;
        if require_binaries && plan.source_path.is_none() {
            let key = env_key(&tool.name);
            bail!(
                "NativePending: official artifact {} is not vendored for deploy; run `phoxal update`, set PHOXAL_ARTIFACT_{key}_PATH, set PHOXAL_ARTIFACT_DIR, set PHOXAL_TOOL_{key}_PATH, or set PHOXAL_TOOL_DIR",
                tool.name,
            );
        }
        artifacts.insert(tool.name.clone(), plan);
    }
    Ok(artifacts)
}

/// The filesystem-safe projection of a provider-qualified package id
/// (`phoxal/service-drive` -> `phoxal-service-drive`), used for on-disk
/// binary/install names - a package id's `/` is not a legal path component.
fn filesystem_safe_package_name(package: &str) -> String {
    package.replace('/', "-")
}

fn official_runtime_plan(
    _root: &Path,
    runtime: &ResolvedPlatformRuntime,
    _test_stub_missing: bool,
) -> Result<OfficialArtifactPlan> {
    let descriptor = crate::native_artifacts::NativeArtifactDescriptor::from_runtime(runtime)?;
    let install_binary_name = filesystem_safe_package_name(&runtime.package);
    let (url, size, sha256, target, archive_binary_name) = descriptor.map_or_else(
        || {
            (
                String::new(),
                0,
                runtime.sha256.clone().unwrap_or_else(|| "0".repeat(64)),
                runtime
                    .target
                    .clone()
                    .unwrap_or_else(|| "assets".to_string()),
                crate::resolver::official_binary_name(runtime.kind, &runtime.name),
            )
        },
        |descriptor| {
            (
                descriptor.url,
                descriptor.size,
                descriptor.sha256,
                descriptor.target.unwrap_or_else(|| "assets".to_string()),
                descriptor.binary_name,
            )
        },
    );
    Ok(OfficialArtifactPlan {
        artifact_id: runtime.package.clone(),
        kind: runtime.kind,
        version: runtime.version.clone(),
        sha256,
        url,
        size,
        target,
        archive_binary_name,
        install_binary_name,
        source_path: None,
        missing_label: (!runtime.published).then(|| format!("{} (missing)", runtime.package)),
    })
}

fn official_tool_plan(_root: &Path, tool: &ResolvedTool) -> Result<OfficialArtifactPlan> {
    let descriptor = crate::native_artifacts::NativeArtifactDescriptor::from_tool(tool)?;
    let (url, size, sha256, target, archive_binary_name) = descriptor.map_or_else(
        || {
            (
                String::new(),
                0,
                tool.sha256.clone(),
                tool.target.clone(),
                tool.binary_name.clone(),
            )
        },
        |descriptor| {
            (
                descriptor.url,
                descriptor.size,
                descriptor.sha256,
                descriptor.target.unwrap_or_else(|| "assets".to_string()),
                descriptor.binary_name,
            )
        },
    );
    Ok(OfficialArtifactPlan {
        artifact_id: tool.package.clone(),
        kind: tool.kind,
        version: tool.resolved.clone(),
        sha256,
        url,
        size,
        target,
        archive_binary_name,
        install_binary_name: tool.name.clone(),
        source_path: None,
        missing_label: (!tool.published).then(|| format!("{} (missing)", tool.package)),
    })
}

fn env_key(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect()
}

fn render_env_files(
    root: &Path,
    plan: &LaunchPlan,
    env_files: &mut BTreeMap<String, String>,
) -> Result<()> {
    let robot = plan
        .robots
        .first()
        .context("deploy launch plan has no robot")?;
    for site in &plan.site {
        if site.id == SITE_INFRASTRUCTURE_ROUTER {
            continue;
        }
        // Every OTHER standard site tool (`tool-joypad`, `tool-telemetry`) -
        // a real bus client, so unlike the router (transport-only, no
        // `PHOXAL_CONNECT` of its own) it needs the same connect endpoint
        // every regular participant gets from `launch_plan::participant_launch`.
        let encoded = site_tool_env(site, &robot.namespace, &robot.id)?;
        write_env_file(root, &format!("{}.env", site.id), &encoded, env_files)?;
    }

    for participant in &robot.participants {
        let encoded = encode_participant_env(&participant.launch)?;
        write_env_file(
            root,
            &format!("{}.env", participant.launch.participant_id),
            &encoded,
            env_files,
        )?;
    }
    Ok(())
}

/// The env for every standard site tool OTHER than the router (`tool-joypad`,
/// `tool-telemetry`) - a real bus client, unlike the router itself, so it
/// needs `PHOXAL_CONNECT` set to the same `DEFAULT_ROUTER_CONNECT` every
/// regular participant's `ParticipantLaunch.bus.connect_endpoints` carries
/// (`launch_plan::participant_launch`).
fn site_tool_env(
    site: &SiteLaunch,
    namespace: &str,
    robot_id: &str,
) -> Result<EncodedParticipantEnv> {
    let mut variables = BTreeMap::new();
    variables.insert(env::PARTICIPANT_ID.to_string(), site.id.clone());
    variables.insert(env::NAMESPACE.to_string(), namespace.to_string());
    variables.insert(env::ROBOT_ID.to_string(), robot_id.to_string());
    variables.insert(env::ROBOT_ROOT.to_string(), ACTIVE_ROOT.to_string());
    // Configless tools (`phoxal_config == Value::Null`, e.g. joypad/telemetry)
    // must run with `PHOXAL_CONFIG` ABSENT - a unit config (`type Config = ()`)
    // rejects `{}`. Only a tool carrying real config emits the variable.
    if !site.phoxal_config.is_null() {
        variables.insert(
            env::CONFIG.to_string(),
            serde_json::to_string(&site.phoxal_config)
                .with_context(|| format!("failed to encode PHOXAL_CONFIG for {}", site.id))?,
        );
    }
    variables.insert(env::CONNECT.to_string(), DEFAULT_ROUTER_CONNECT.to_string());
    Ok(EncodedParticipantEnv::from_variables(variables))
}

fn write_env_file(
    root: &Path,
    file_name: &str,
    encoded: &EncodedParticipantEnv,
    env_files: &mut BTreeMap<String, String>,
) -> Result<()> {
    let rendered = encoded.environment_file();
    write_text(&payload_env(root).join(file_name), &rendered)?;
    env_files.insert(format!("{OPT_ENV}/{file_name}"), rendered);
    Ok(())
}

fn render_units(
    root: &Path,
    resolved: &ResolvedRobot,
    plan: &LaunchPlan,
    source_builds: &BTreeMap<String, SourceBuildArtifact>,
    official_plans: &BTreeMap<String, OfficialArtifactPlan>,
    rendered_units: &mut BTreeMap<String, String>,
) -> Result<Vec<String>> {
    let mut unit_names = Vec::new();
    write_unit(root, "phoxal.target", &target_unit(), rendered_units)?;
    unit_names.push("phoxal.target".to_string());

    let router_binary = official_plans
        .get(SITE_INFRASTRUCTURE_ROUTER)
        .map(|tool| tool.install_binary_name.clone())
        .unwrap_or_else(|| SITE_INFRASTRUCTURE_ROUTER.to_string());
    write_unit(
        root,
        "phoxal-router.service",
        &router_unit(&router_binary),
        rendered_units,
    )?;
    unit_names.push("phoxal-router.service".to_string());

    // Every OTHER standard site tool (`tool-joypad`, `tool-telemetry` -
    // CLI-UX Phase 4) gets its own unit too, ordered AFTER the router
    // (`site_tool_unit`'s `After=`/`Wants=`) and matching the staged
    // readiness the CLI supervisor itself uses (router before the other
    // tools - `commands::run::stages_for_run`). `plan.site` already omits
    // `tool-telemetry` entirely when the catalog snapshot in use predates it
    // (`launch_plan::build_site_launches`), so this loop never renders a unit
    // for a tool that was never resolved.
    for site in &plan.site {
        if site.id == SITE_INFRASTRUCTURE_ROUTER {
            continue;
        }
        let unit_name = site_tool_unit_name(&site.id);
        let binary = if source_builds.contains_key(&site.id) {
            site.id.clone()
        } else {
            official_plans
                .get(&site.id)
                .map(|artifact| artifact.install_binary_name.clone())
                .unwrap_or_else(|| site.id.clone())
        };
        let privileges = unit_privileges_for_tool(&site.id);
        write_unit(
            root,
            &unit_name,
            &site_tool_unit(&site.id, &binary, &privileges),
            rendered_units,
        )?;
        unit_names.push(unit_name);
    }

    let robot = plan
        .robots
        .first()
        .context("deploy launch plan has no robot")?;
    for participant in &robot.participants {
        let unit_name = participant_unit_name(&participant.launch.participant_id);
        let binary = participant_binary_name(participant, resolved, source_builds, official_plans)?;
        let privileges = unit_privileges(resolved, &participant.launch.participant_id);
        write_unit(
            root,
            &unit_name,
            &participant_unit(participant, &binary, &privileges),
            rendered_units,
        )?;
        unit_names.push(unit_name);
    }
    Ok(unit_names)
}

fn write_unit(
    root: &Path,
    unit_name: &str,
    contents: &str,
    rendered_units: &mut BTreeMap<String, String>,
) -> Result<()> {
    write_text(&payload_systemd(root).join(unit_name), contents)?;
    rendered_units.insert(format!("{SYSTEMD_DIR}/{unit_name}"), contents.to_string());
    Ok(())
}

fn target_unit() -> String {
    "[Unit]\nDescription=Phoxal robot\nWants=phoxal-router.service\n\n[Install]\nWantedBy=multi-user.target\n".to_string()
}

fn router_unit(binary: &str) -> String {
    format!(
        "[Unit]\nDescription=Phoxal Zenoh router\nAfter=network-online.target\nWants=network-online.target\nPartOf=phoxal.target\nStartLimitIntervalSec={}\nStartLimitBurst={START_LIMIT_BURST}\n\n[Service]\nType=simple\nExecStart={OPT_BIN}/{binary} --listen tcp/localhost:7447\nRestart=on-failure\nRestartSec=2s\nTimeoutStopSec=5s\nUser=phoxal\nGroup=phoxal\nNoNewPrivileges=true\n\n[Install]\nWantedBy=phoxal.target\n",
        START_LIMIT_INTERVAL.as_secs()
    )
}

/// The unit for a standard site tool OTHER than the router (`tool-joypad`,
/// `tool-telemetry`) - shaped exactly like `participant_unit` (same
/// `Type=notify` readiness contract, same restart/watchdog/hardening
/// defaults: no `MemoryMax`/`CPUQuota` here either, consistent with every
/// other unit this deploy renders) but ordered after the router by unit
/// name rather than a `ParticipantLaunchRecord`, since a site tool has no
/// graph-checked participant record of its own.
///
/// No-controller idle policy (design doc): a site tool with nothing to do
/// yet (`tool-joypad` with no gamepad plugged in) is expected to start and
/// idle cleanly rather than exit - the framework tool itself already stays
/// up in that case (see `tool/joypad`'s own graceful-absence handling), so
/// `Restart=on-failure` never actually flaps for it; this unit adds no
/// additional restart-suppression logic because none is needed.
fn site_tool_unit(id: &str, binary: &str, privileges: &UnitPrivileges) -> String {
    let mut unit = format!(
        "[Unit]\nDescription=Phoxal tool {id}\nAfter=network-online.target phoxal-router.service\nWants=network-online.target\nPartOf=phoxal.target\nStartLimitIntervalSec={}\nStartLimitBurst={START_LIMIT_BURST}\n\n[Service]\nType=notify\nEnvironmentFile={OPT_ENV}/{id}.env\nExecStart={OPT_BIN}/{binary}\n\nRestart=on-failure\nRestartSec=2s\nTimeoutStopSec=5s\nWatchdogSec={WATCHDOG_SEC}s\n\nUser=phoxal\nGroup=phoxal\nNoNewPrivileges=true\n",
        START_LIMIT_INTERVAL.as_secs()
    );
    if !privileges.supplementary_groups.is_empty() {
        unit.push_str("SupplementaryGroups=");
        unit.push_str(&privileges.supplementary_groups.join(" "));
        unit.push('\n');
    }
    if !privileges.device_allow.is_empty() {
        unit.push_str("DevicePolicy=strict\n");
        for device in &privileges.device_allow {
            unit.push_str("DeviceAllow=");
            unit.push_str(device);
            unit.push_str(" rw\n");
        }
    }
    if !privileges.capabilities.is_empty() {
        let caps = privileges.capabilities.join(" ");
        unit.push_str("AmbientCapabilities=");
        unit.push_str(&caps);
        unit.push('\n');
        unit.push_str("CapabilityBoundingSet=");
        unit.push_str(&caps);
        unit.push('\n');
    }
    unit.push_str("\n[Install]\nWantedBy=phoxal.target\n");
    unit
}

fn site_tool_unit_name(id: &str) -> String {
    format!("phoxal-{id}.service")
}

/// The tool-privilege model (CLI-UX Phase 4): a second, hardcoded privilege
/// path for a standard site tool, alongside `unit_privileges`'s
/// component-driver-derived path for a robot participant. `tool-joypad`
/// needs `/dev/input` access to read gamepad hardware - granted
/// unconditionally (the `input` supplementary group plus
/// `DeviceAllow=/dev/input/* rw`) for the current development-grade robots,
/// with no manifest/config switch to gate it (design doc). Every other site
/// tool (`tool-telemetry`; the router has its own always-privilege-free unit)
/// needs no extra grant.
fn unit_privileges_for_tool(tool_id: &str) -> UnitPrivileges {
    if tool_id == SITE_TOOL_JOYPAD {
        UnitPrivileges {
            supplementary_groups: vec!["input".to_string()],
            device_allow: vec!["/dev/input/*".to_string()],
            capabilities: Vec::new(),
        }
    } else {
        UnitPrivileges::default()
    }
}

fn participant_unit(
    participant: &ParticipantLaunchRecord,
    binary: &str,
    privileges: &UnitPrivileges,
) -> String {
    let id = &participant.launch.participant_id;
    let mut unit = format!(
        "[Unit]\nDescription=Phoxal participant {id}\nAfter=network-online.target phoxal-router.service\nWants=network-online.target\nPartOf=phoxal.target\nStartLimitIntervalSec={}\nStartLimitBurst={START_LIMIT_BURST}\n\n[Service]\nType=notify\nEnvironmentFile={OPT_ENV}/{id}.env\nExecStart={OPT_BIN}/{binary}\n\nRestart=on-failure\nRestartSec=2s\nTimeoutStopSec=5s\nStateDirectory=phoxal\nWatchdogSec={WATCHDOG_SEC}s\n\nUser=phoxal\nGroup=phoxal\nNoNewPrivileges=true\n",
        START_LIMIT_INTERVAL.as_secs()
    );
    if !privileges.supplementary_groups.is_empty() {
        unit.push_str("SupplementaryGroups=");
        unit.push_str(&privileges.supplementary_groups.join(" "));
        unit.push('\n');
    }
    if !privileges.device_allow.is_empty() {
        unit.push_str("DevicePolicy=strict\n");
        for device in &privileges.device_allow {
            unit.push_str("DeviceAllow=");
            unit.push_str(device);
            unit.push_str(" rw\n");
        }
    }
    if !privileges.capabilities.is_empty() {
        let caps = privileges.capabilities.join(" ");
        unit.push_str("AmbientCapabilities=");
        unit.push_str(&caps);
        unit.push('\n');
        unit.push_str("CapabilityBoundingSet=");
        unit.push_str(&caps);
        unit.push('\n');
    }
    unit.push_str("\n[Install]\nWantedBy=phoxal.target\n");
    unit
}

fn participant_unit_name(participant_id: &str) -> String {
    format!("phoxal-participant-{participant_id}.service")
}

fn participant_binary_name(
    participant: &ParticipantLaunchRecord,
    resolved: &ResolvedRobot,
    source_builds: &BTreeMap<String, SourceBuildArtifact>,
    official_plans: &BTreeMap<String, OfficialArtifactPlan>,
) -> Result<String> {
    match &participant.execution {
        ParticipantExecution::UserService { .. } => Ok(participant.artifact_id.clone()),
        ParticipantExecution::SourceArtifact { kind, .. } if kind == "service" => {
            Ok(format!("service-{}", participant.artifact_id))
        }
        ParticipantExecution::SourceArtifact { kind, .. } => {
            Ok(format!("{kind}-{}", participant.artifact_id))
        }
        ParticipantExecution::ComponentDriver { .. } => {
            Ok(format!("driver-{}", participant.artifact_id))
        }
        ParticipantExecution::OfficialArtifact { .. } => {
            let runtime = official_runtime_by_artifact_id(resolved, &participant.artifact_id)
                .ok_or_else(|| {
                    anyhow!(
                        "official participant {} has no resolved runtime",
                        participant.artifact_id
                    )
                })?;
            official_plans
                .get(&runtime.package)
                .map(|artifact| artifact.install_binary_name.clone())
                .ok_or_else(|| {
                    anyhow!(
                        "official participant {} has no staged artifact plan",
                        participant.artifact_id
                    )
                })
        }
    }
    .and_then(|binary| {
        if source_builds.contains_key(&binary)
            || official_plans.contains_key(&binary)
            || !binary.is_empty()
        {
            Ok(binary)
        } else {
            bail!(
                "participant {} resolved an empty binary name",
                participant.launch.participant_id
            )
        }
    })
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct UnitPrivileges {
    supplementary_groups: Vec<String>,
    device_allow: Vec<String>,
    capabilities: Vec<String>,
}

fn unit_privileges(resolved: &ResolvedRobot, participant_id: &str) -> UnitPrivileges {
    let Some(component) = resolved.robot.robot.components.get(participant_id) else {
        return UnitPrivileges::default();
    };
    let Some(driver) = component.driver.as_ref() else {
        return UnitPrivileges::default();
    };
    let mut privileges = match &driver.connection {
        ConnectionConfig::Can { bus, .. } => UnitPrivileges {
            supplementary_groups: Vec::new(),
            device_allow: Vec::new(),
            capabilities: vec!["CAP_NET_RAW".to_string()],
        }
        .with_note_device(format!("/sys/class/net/can{bus}")),
        ConnectionConfig::I2c { bus, .. } => UnitPrivileges {
            supplementary_groups: vec!["i2c".to_string()],
            device_allow: vec![format!("/dev/i2c-{bus}")],
            capabilities: Vec::new(),
        },
        ConnectionConfig::Spi { bus, chip_select } => UnitPrivileges {
            supplementary_groups: vec!["spi".to_string()],
            device_allow: vec![format!("/dev/spidev{bus}.{chip_select}")],
            capabilities: Vec::new(),
        },
        ConnectionConfig::Serial { port, .. } | ConnectionConfig::Uart { port, .. } => {
            UnitPrivileges {
                supplementary_groups: vec!["dialout".to_string()],
                device_allow: vec![port.clone()],
                capabilities: Vec::new(),
            }
        }
        ConnectionConfig::Usb { .. } => UnitPrivileges {
            supplementary_groups: vec!["plugdev".to_string(), "video".to_string()],
            device_allow: Vec::new(),
            capabilities: Vec::new(),
        },
        ConnectionConfig::Gpio { chip, .. } => UnitPrivileges {
            supplementary_groups: vec!["gpio".to_string()],
            device_allow: vec![if chip.starts_with('/') {
                chip.clone()
            } else {
                format!("/dev/{chip}")
            }],
            capabilities: Vec::new(),
        },
    };
    privileges.sort_dedup();
    privileges
}

impl UnitPrivileges {
    fn with_note_device(self, _device: String) -> Self {
        self
    }

    fn sort_dedup(&mut self) {
        self.supplementary_groups.sort();
        self.supplementary_groups.dedup();
        self.device_allow.sort();
        self.device_allow.dedup();
        self.capabilities.sort();
        self.capabilities.dedup();
    }
}

fn write_robot_yaml(root: &Path, robot: &Robot) -> Result<()> {
    // Serialize through the version dispatcher so the payload keeps the
    // `schema: robot/v0` tag - on-target consumers (tool-router, participants)
    // parse via the dispatcher and reject an untagged manifest.
    let yaml = serde_yaml::to_string(&phoxal::model::robot::Robot::V0(robot.clone()))
        .context("failed to serialize resolved robot.yaml")?;
    write_text(&payload_opt(root).join("robot.yaml"), &yaml)
}

/// Stage the robot's own structure file plus every resolved component's full
/// `component_assets` bundle (`component.yaml`, `simulation.yaml`,
/// `structure.urdf`, `meshes/`) into the deploy payload's
/// `/opt/phoxal/components/<component-id>/` (docs #21's deploy install
/// payload shape). One component id may back several instances; its bundle is
/// staged once and shared. Asset resolution never depends on where a
/// component checkout happened to live - only the resolved
/// `component_assets` package's own source directory matters.
///
/// `Path`-sourced (dev-overlay-pinned) assets stage from their local checkout
/// directly. `Catalog`-sourced assets stage from the CLI's native-artifact
/// cache ONLY - never the network, matching the pre-existing offline
/// guarantee official service/tool binaries already have (`phoxal update`
/// populates the cache; deploy, live or `--dry-run`, only ever reads it). A
/// `Git`-sourced assets package is not staged here (no local checkout in the
/// deploy flow) - unchanged pre-existing scope.
fn stage_payload_metadata(
    project_root: &Path,
    root: &Path,
    robot: &Robot,
    resolved: &ResolvedRobot,
) -> Result<Vec<String>> {
    let reads_catalog_assets = resolved.components.iter().any(|component| {
        component
            .assets
            .as_ref()
            .is_some_and(|assets| matches!(&assets.source, ResolvedComponentSource::Catalog))
    });
    let _artifact_lock = (reads_catalog_assets
        && crate::host_paths::artifacts_dir().is_ok_and(|path| path.is_dir()))
    .then(crate::native_artifacts::ArtifactStoreLock::shared)
    .transpose()?;
    let mut staged_files = Vec::new();
    staged_files.push(stage_declared_metadata_file(
        project_root,
        &payload_opt(root),
        &robot.robot.structure,
        "robot structure",
    )?);

    let mut staged_components = BTreeSet::new();
    for component in &resolved.components {
        let component_id = &component.source_name;
        // A driverless (passive) component with no official assets package
        // has nothing to stage here - `component.assets` is `None`.
        let source_dir = match component.assets.as_ref() {
            None => None,
            Some(assets) => match &assets.source {
                ResolvedComponentSource::Path { .. } => {
                    crate::component_driver::component_assets_dir(component, project_root)?
                }
                ResolvedComponentSource::Catalog => locate_cached_component_assets_dir(assets)?,
                ResolvedComponentSource::Git { .. } => None,
            },
        };
        let Some(source_dir) = source_dir else {
            continue;
        };
        if !staged_components.insert(component_id.clone()) {
            continue;
        }
        let payload_dir = payload_components_dir(root).join(component_id);
        staged_files.extend(stage_component_assets_bundle(
            root,
            &source_dir,
            &payload_dir,
            component_id,
        )?);
    }

    Ok(staged_files)
}

/// Locate a Catalog-sourced `component_assets` package's already-staged cache
/// directory, WITHOUT downloading - deploy (live or `--dry-run`) must never
/// reach the network for artifacts; `phoxal update` populates this
/// cache. `Ok(None)` when nothing is cached yet (a fresh clean machine that
/// has not run `update`) - the caller skips staging that component's assets
/// rather than erroring, since deploy's live-artifact `NativePending` gate
/// (`stage_official_artifacts`) is the authoritative "not available locally"
/// diagnostic surface; a missing assets-only bundle does not block a
/// hardware-driver deploy.
fn locate_cached_component_assets_dir(
    package: &crate::resolver::ResolvedComponentPackage,
) -> Result<Option<PathBuf>> {
    let Some(runtime) = &package.catalog_runtime else {
        return Ok(None);
    };
    let Some(descriptor) =
        crate::native_artifacts::NativeArtifactDescriptor::from_runtime(runtime)?
    else {
        return Ok(None);
    };
    let exec_dir = crate::native_artifacts::artifact_exec_dir(&descriptor)?;
    Ok(exec_dir.is_dir().then_some(exec_dir))
}

fn payload_components_dir(root: &Path) -> PathBuf {
    payload_opt(root).join("components")
}

/// Copy one component's asset bundle (`component.yaml`, `simulation.yaml`,
/// `structure.urdf`, `meshes/`) from its resolved source directory into the
/// staged payload directory, returning the `/opt/phoxal/...`-rooted remote
/// paths of every file staged. Only files that actually exist are copied -
/// `simulation.yaml` and `meshes/` are optional per component.
fn stage_component_assets_bundle(
    root: &Path,
    source_dir: &Path,
    payload_dir: &Path,
    component_id: &str,
) -> Result<Vec<String>> {
    let mut staged_files = Vec::new();
    let component_file = source_dir.join(COMPONENT_FILE);
    copy_metadata_file(
        &component_file,
        &payload_dir.join(COMPONENT_FILE),
        &format!("component metadata for '{component_id}'"),
    )?;
    staged_files.push(payload_remote_path(root, &payload_dir.join(COMPONENT_FILE)));

    for optional_file in [STRUCTURE_FILE, SIMULATION_FILE] {
        let source_file = source_dir.join(optional_file);
        if !source_file.is_file() {
            continue;
        }
        copy_metadata_file(
            &source_file,
            &payload_dir.join(optional_file),
            &format!("component {optional_file} for '{component_id}'"),
        )?;
        staged_files.push(payload_remote_path(root, &payload_dir.join(optional_file)));
    }

    let meshes_source = source_dir.join(MESHES_DIR);
    if meshes_source.is_dir() {
        let meshes_dest = payload_dir.join(MESHES_DIR);
        copy_dir_recursive(root, &meshes_source, &meshes_dest, &mut staged_files)?;
    }

    Ok(staged_files)
}

/// The remote path a staged payload file is reported under, computed by
/// stripping the temp payload root (`payload_opt(root)`) and re-rooting under
/// `{OPT_ROOT}`.
fn payload_remote_path(root: &Path, payload_path: &Path) -> String {
    let relative = payload_path
        .strip_prefix(payload_opt(root))
        .unwrap_or(payload_path);
    opt_remote_path(relative)
}

fn copy_dir_recursive(
    root: &Path,
    source: &Path,
    dest: &Path,
    staged_files: &mut Vec<String>,
) -> Result<()> {
    fs::create_dir_all(dest).with_context(|| format!("failed to create {}", dest.display()))?;
    for entry in
        fs::read_dir(source).with_context(|| format!("failed to read {}", source.display()))?
    {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let source_path = entry.path();
        let dest_path = dest.join(entry.file_name());
        if file_type.is_dir() {
            copy_dir_recursive(root, &source_path, &dest_path, staged_files)?;
        } else if file_type.is_file() {
            fs::copy(&source_path, &dest_path).with_context(|| {
                format!(
                    "failed to stage {} to {}",
                    source_path.display(),
                    dest_path.display()
                )
            })?;
            staged_files.push(payload_remote_path(root, &dest_path));
        }
    }
    Ok(())
}

fn stage_declared_metadata_file(
    project_root: &Path,
    payload_root: &Path,
    relative_path: &Path,
    label: &str,
) -> Result<String> {
    validate_payload_relative_path(relative_path, label)?;
    let source = crate::utils::resolve_project_path(project_root, relative_path);
    copy_metadata_file(&source, &payload_root.join(relative_path), label)?;
    Ok(opt_remote_path(relative_path))
}

fn validate_payload_relative_path(path: &Path, field: &str) -> Result<()> {
    if path.as_os_str().is_empty() {
        bail!("{field} must not be empty");
    }
    if path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                PathComponent::ParentDir | PathComponent::RootDir | PathComponent::Prefix(_)
            )
        })
    {
        bail!(
            "{field} path '{}' must stay within {OPT_ROOT}",
            path.display()
        );
    }
    Ok(())
}

fn copy_metadata_file(source: &Path, dest: &Path, label: &str) -> Result<()> {
    if !source.is_file() {
        bail!("{label} file {} does not exist", source.display());
    }
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    fs::copy(source, dest).with_context(|| {
        format!(
            "failed to stage {label} {} to {}",
            source.display(),
            dest.display()
        )
    })?;
    Ok(())
}

fn opt_remote_path(relative_path: &Path) -> String {
    format!("{ACTIVE_ROOT}/{}", relative_path.display())
}

fn release_record(
    resolved: &ResolvedRobot,
    plan: &LaunchPlan,
    source_builds: &BTreeMap<String, SourceBuildArtifact>,
    official_plans: &BTreeMap<String, OfficialArtifactPlan>,
) -> Result<ReleaseRecord> {
    let mut artifacts = BTreeMap::<String, ReleaseArtifact>::new();
    if let Some(router) = official_plans.get(SITE_INFRASTRUCTURE_ROUTER) {
        artifacts.insert(
            router.artifact_id.clone(),
            release_official_artifact(router),
        );
    }

    for participant in &plan.robots[0].participants {
        match &participant.execution {
            ParticipantExecution::OfficialArtifact { .. } => {
                let runtime = official_runtime_by_artifact_id(resolved, &participant.artifact_id)
                    .ok_or_else(|| {
                    anyhow!("missing runtime for {}", participant.artifact_id)
                })?;
                if let Some(artifact) = official_plans.get(&runtime.package) {
                    artifacts
                        .entry(artifact.artifact_id.clone())
                        .or_insert_with(|| release_official_artifact(artifact));
                }
            }
            ParticipantExecution::UserService { .. } => {
                if let Some(artifact) = source_builds.get(&participant.artifact_id) {
                    artifacts
                        .entry(artifact.artifact_id.clone())
                        .or_insert_with(|| release_source_artifact(artifact));
                }
            }
            ParticipantExecution::SourceArtifact { kind, .. } if kind == "service" => {
                let id = format!("service-{}", participant.artifact_id);
                if let Some(artifact) = source_builds.get(&id) {
                    artifacts
                        .entry(artifact.artifact_id.clone())
                        .or_insert_with(|| release_source_artifact(artifact));
                }
            }
            ParticipantExecution::SourceArtifact { kind, .. } => {
                let id = format!("{kind}-{}", participant.artifact_id);
                if let Some(artifact) = source_builds.get(&id) {
                    artifacts
                        .entry(artifact.artifact_id.clone())
                        .or_insert_with(|| release_source_artifact(artifact));
                }
            }
            ParticipantExecution::ComponentDriver { .. } => {
                let id = format!("driver-{}", participant.artifact_id);
                if let Some(artifact) = source_builds.get(&id) {
                    artifacts
                        .entry(artifact.artifact_id.clone())
                        .or_insert_with(|| release_source_artifact(artifact));
                }
            }
        }
    }

    Ok(ReleaseRecord {
        schema: RELEASE_SCHEMA.to_string(),
        created_at_utc: utc_now_string()?,
        artifacts: artifacts.into_values().collect(),
    })
}

fn release_source_artifact(artifact: &SourceBuildArtifact) -> ReleaseArtifact {
    let _ = &artifact.payload_path;
    ReleaseArtifact {
        id: artifact.artifact_id.clone(),
        kind: artifact.kind.to_string(),
        version: None,
        source: artifact.source.clone(),
        sha256: artifact.sha256.clone(),
        target: None,
        url: None,
    }
}

fn release_official_artifact(artifact: &OfficialArtifactPlan) -> ReleaseArtifact {
    ReleaseArtifact {
        id: artifact.artifact_id.clone(),
        kind: artifact.kind.to_string(),
        version: Some(artifact.version.clone()),
        source: Value::String("catalog".to_string()),
        sha256: artifact.sha256.clone(),
        target: Some(artifact.target.clone()),
        url: Some(artifact.url.clone()),
    }
}

fn utc_now_string() -> Result<String> {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before Unix epoch")?
        .as_secs() as i64;
    Ok(format_unix_utc(secs))
}

fn format_unix_utc(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let seconds_of_day = secs.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let hour = seconds_of_day / 3_600;
    let minute = (seconds_of_day % 3_600) / 60;
    let second = seconds_of_day % 60;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

fn civil_from_days(days_since_epoch: i64) -> (i32, u32, u32) {
    let z = days_since_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = mp + if mp < 10 { 3 } else { -9 };
    let year = y + if m <= 2 { 1 } else { 0 };
    (year as i32, m as u32, d as u32)
}

fn sha256_file(path: &Path) -> Result<String> {
    let bytes = fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
    Ok(hex::encode(Sha256::digest(bytes)))
}

fn helper_script_sha256() -> String {
    hex::encode(Sha256::digest(helper_script().as_bytes()))
}

fn write_text(path: &Path, contents: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    fs::write(path, contents).with_context(|| format!("failed to write {}", path.display()))
}

fn helper_script() -> String {
    format!(
        r#"#!/bin/sh
set -eu
unit_dir="{SYSTEMD_DIR}"
opt_root="{OPT_ROOT}"
releases_root="{RELEASES_ROOT}"

valid_unit() {{
  case "$1" in
    phoxal.target|phoxal-router.service|phoxal-tool-*.service|phoxal-participant-*.service) ;;
    *) return 1 ;;
  esac
  case "$1" in
    *[!A-Za-z0-9_.@-]*) return 1 ;;
  esac
  return 0
}}

valid_stage_suffix() {{
  case "$1" in
    ""|*[!A-Za-z0-9_.@-]*) return 1 ;;
  esac
  return 0
}}

valid_payload_source() {{
  case "$1" in
    {PAYLOAD_STAGING_PREFIX}*) ;;
    *) return 1 ;;
  esac
  suffix="${{1#{PAYLOAD_STAGING_PREFIX}}}"
  valid_stage_suffix "$suffix"
}}

valid_generation() {{
  case "$1" in
    ""|*[!A-Fa-f0-9]*) return 1 ;;
  esac
  [ "${{#1}}" -eq 16 ]
}}

valid_name() {{
  case "$1" in
    ""|*[!A-Za-z0-9_.@+-]*) return 1 ;;
  esac
}}

release_partial() {{
  printf '%s/%s.partial' "$releases_root" "$1"
}}

case "${{1:-}}" in
  prepare-release)
    source="${{2:-}}"
    generation="${{3:-}}"
    valid_payload_source "$source" || exit 64
    valid_generation "$generation" || exit 64
    test -d "$source"
    partial="$(release_partial "$generation")"
    install -d -o phoxal -g phoxal -m 0755 "$opt_root" "$releases_root"
    rm -rf "$partial"
    install -d -o phoxal -g phoxal -m 0755 "$partial"
    cp -a "$source/." "$partial/"
    chown -R phoxal:phoxal "$partial"
    ;;
  download-artifact)
    generation="${{2:-}}"
    expected_size="${{3:-}}"
    expected_sha="${{4:-}}"
    archive_binary="${{5:-}}"
    install_binary="${{6:-}}"
    valid_generation "$generation" || exit 64
    valid_name "$archive_binary" || exit 64
    valid_name "$install_binary" || exit 64
    case "$expected_size" in ""|*[!0-9]*) exit 64 ;; esac
    case "$expected_sha" in ""|*[!a-f0-9]*) exit 64 ;; esac
    [ "${{#expected_sha}}" -eq 64 ]
    url="$(cat)"
    case "$url" in https://*) ;; *) exit 64 ;; esac
    partial_root="$(release_partial "$generation")"
    test -d "$partial_root"
    downloads="$partial_root/.downloads"
    install -d -o phoxal -g phoxal -m 0755 "$downloads" "$partial_root/bin"
    partial="$downloads/$expected_sha.partial"
    archive="$downloads/$expected_sha.archive"
    unpack="$downloads/$expected_sha.unpack"
    rm -f "$partial" "$archive"
    rm -rf "$unpack"
    curl --fail --location --silent --show-error --retry {curl_retries} --retry-all-errors --connect-timeout 10 --max-time 120 --output "$partial" "$url"
    actual_size="$(wc -c < "$partial" | tr -d ' ')"
    [ "$actual_size" = "$expected_size" ]
    actual_sha="$(sha256sum "$partial" | awk '{{print $1}}')"
    [ "$actual_sha" = "$expected_sha" ]
    mv "$partial" "$archive"
    install -d -o phoxal -g phoxal -m 0755 "$unpack"
    tar -xf "$archive" -C "$unpack"
    binary="$(find "$unpack" -type f -name "$archive_binary" -print -quit)"
    test -n "$binary"
    install -o phoxal -g phoxal -m 0755 "$binary" "$partial_root/bin/$install_binary"
    rm -rf "$archive" "$unpack"
    ;;
  activate-release)
    generation="${{2:-}}"
    valid_generation "$generation" || exit 64
    partial="$(release_partial "$generation")"
    release="$releases_root/$generation"
    test -d "$partial"
    test ! -e "$release"
    rm -rf "$partial/.downloads"
    mv "$partial" "$release"
    old="$(readlink "$opt_root/active" 2>/dev/null || true)"
    if [ -n "$old" ]; then
      ln -s "$old" "$opt_root/.previous.partial"
      mv -Tf "$opt_root/.previous.partial" "$opt_root/previous"
    fi
    ln -s "releases/$generation" "$opt_root/.active.partial"
    mv -Tf "$opt_root/.active.partial" "$opt_root/active"
    ;;
  rollback-release)
    previous="$(readlink "$opt_root/previous" 2>/dev/null || true)"
    test -n "$previous"
    failed="$(readlink "$opt_root/active")"
    ln -s "$previous" "$opt_root/.active.partial"
    mv -Tf "$opt_root/.active.partial" "$opt_root/active"
    ln -s "$failed" "$opt_root/.previous.partial"
    mv -Tf "$opt_root/.previous.partial" "$opt_root/previous"
    systemctl daemon-reload
    ;;
  install-unit)
    unit="${{2:-}}"
    valid_unit "$unit"
    ln -sfn "{ACTIVE_ROOT}/systemd/$unit" "$unit_dir/$unit"
    ;;
  remove-unit)
    unit="${{2:-}}"
    valid_unit "$unit"
    rm -f "$unit_dir/$unit"
    ;;
  enable-unit)
    unit="${{2:-}}"
    valid_unit "$unit"
    systemctl enable "$unit"
    ;;
  disable-unit)
    unit="${{2:-}}"
    valid_unit "$unit"
    systemctl disable "$unit" || true
    ;;
  daemon-reload)
    systemctl daemon-reload
    ;;
  restart-target)
    systemctl reset-failed 'phoxal*' || true
    systemctl restart phoxal.target
    ;;
  *)
    exit 64
    ;;
esac
"#,
        curl_retries = DOWNLOAD_RETRIES - 1,
    )
}

/// The grant is a static group rule, not a per-user line: every deploying
/// user is enrolled into the `phoxal-deploy` group instead of the sudoers
/// fragment naming a user directly, so a second operator's deploy no longer
/// silently revokes the first operator's grant by rewriting the fragment.
fn sudoers_fragment() -> String {
    format!("%phoxal-deploy ALL=(root) NOPASSWD: {HELPER_PATH} *\n")
}

/// A conservative allowlist for a username interpolated directly into the
/// bootstrap shell script (`usermod -aG phoxal-deploy <remote_user>`):
/// ASCII alphanumerics plus `_ . @ -`, non-empty. This is intentionally
/// stricter than what some systems permit for usernames - it only needs to
/// admit real remote-login usernames, not every value `useradd` would
/// accept.
fn validate_remote_username(remote_user: &str) -> Result<()> {
    let valid = !remote_user.is_empty()
        && remote_user
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'@' | b'-'));
    if !valid {
        bail!(
            "DeployInvalidRemoteUser: {remote_user:?} is not a valid remote username (expected \
             ASCII letters, digits, or the characters _ . @ -, non-empty); refusing to bootstrap \
             the phoxal-deploy group grant for it"
        );
    }
    Ok(())
}

fn bootstrap_script(scripts: &BootstrapScripts) -> String {
    format!(
        r#"set -eu
if ! getent group phoxal >/dev/null; then
  groupadd --system phoxal
fi
if ! id phoxal >/dev/null 2>&1; then
  useradd --system --gid phoxal --home-dir /var/lib/phoxal --create-home --shell /usr/sbin/nologin phoxal
fi
if ! getent group phoxal-deploy >/dev/null; then
  groupadd --system phoxal-deploy
fi
usermod -aG phoxal-deploy -- {remote_user}
install -d -o phoxal -g phoxal -m 0755 {OPT_ROOT} {RELEASES_ROOT}
install -d -o phoxal -g phoxal -m 0700 {IDENTITY_DIR}
install -d -o phoxal -g phoxal -m 0755 /var/lib/phoxal
cat > {HELPER_PATH} <<'PHOXAL_HELPER'
{helper}
PHOXAL_HELPER
chown root:root {HELPER_PATH}
chmod 0755 {HELPER_PATH}
cat > {SUDOERS_PATH} <<'PHOXAL_SUDOERS'
{sudoers}
PHOXAL_SUDOERS
chown root:root {SUDOERS_PATH}
chmod 0440 {SUDOERS_PATH}
{HELPER_PATH} daemon-reload
systemctl enable phoxal.target || true
"#,
        remote_user = scripts.remote_user,
        helper = scripts.helper_script,
        sudoers = scripts.sudoers_fragment,
    )
}

fn stale_units(installed: &[String], desired: &[String]) -> Vec<String> {
    let desired = desired.iter().map(String::as_str).collect::<BTreeSet<_>>();
    installed
        .iter()
        .filter(|unit| managed_unit_name(unit))
        .filter(|unit| !desired.contains(unit.as_str()))
        .cloned()
        .collect()
}

fn managed_unit_name(unit: &str) -> bool {
    unit == "phoxal.target"
        || unit == "phoxal-router.service"
        || unit
            .strip_prefix("phoxal-tool-")
            .and_then(|rest| rest.strip_suffix(".service"))
            .is_some_and(crate::resolver::is_launch_id)
        || unit
            .strip_prefix("phoxal-participant-")
            .and_then(|rest| rest.strip_suffix(".service"))
            .is_some_and(crate::resolver::is_launch_id)
}

fn report_from_payload(
    mode: &'static str,
    payload: RenderedPayload,
    health: Option<HealthReport>,
) -> DeployReport {
    DeployReport {
        mode,
        target_arch: payload.target.arch,
        official_target_triple: payload.target.official_triple,
        local_target_triple: payload.target.local_triple,
        payload_root: payload.root.path().to_path_buf(),
        install_plan: payload.install_plan,
        rendered_units: payload.rendered_units,
        env_files: payload.env_files,
        release_json: payload.release_json,
        delivery: payload.delivery,
        health,
    }
}

fn report(report: DeployReport) -> Result<()> {
    println!("mode: {}", report.mode);
    println!("target_arch: {}", report.target_arch);
    println!("official_target: {}", report.official_target_triple);
    println!("local_target: {}", report.local_target_triple);
    println!("payload_root: {}", report.payload_root.display());
    println!("install plan:");
    println!("{}", serde_json::to_string_pretty(&report.install_plan)?);
    println!("rendered units:");
    for (path, contents) in &report.rendered_units {
        println!("--- {path}");
        print!("{contents}");
    }
    println!("env files:");
    for (path, contents) in &report.env_files {
        println!("--- {path}");
        print!("{contents}");
    }
    println!("release.json:");
    println!("{}", serde_json::to_string_pretty(&report.release_json)?);
    if let Some(delivery) = report.delivery {
        println!("official_delivery: {delivery:?}");
    }
    if let Some(health) = &report.health {
        println!("health:");
        println!("{}", serde_json::to_string_pretty(health)?);
    }
    Ok(())
}

fn format_health_failure(report: &HealthReport) -> String {
    let mut message = String::from("HealthReportFailed:");
    for unit in report.units.iter().filter(|unit| !unit.ready) {
        message.push_str("\n  - ");
        if let Some(participant) = &unit.participant {
            message.push_str(participant);
            message.push_str(" (");
            message.push_str(&unit.unit);
            message.push(')');
        } else {
            message.push_str(&unit.unit);
        }
        message.push_str(": ");
        message.push_str(&unit.active_state);
        if !unit.sub_state.is_empty() {
            message.push('/');
            message.push_str(&unit.sub_state);
        }
        if !unit.journal_excerpt.is_empty() {
            message.push_str("\n    journal:");
            for line in &unit.journal_excerpt {
                message.push_str("\n      ");
                message.push_str(line);
            }
        }
    }
    message
}

#[derive(Debug, Clone)]
struct SshTransport {
    host: String,
    ui: crate::Ui,
    pending_units: Vec<String>,
}

impl SshTransport {
    fn new(host: String, ui: crate::Ui) -> Self {
        Self {
            host,
            ui,
            pending_units: Vec::new(),
        }
    }

    fn ssh_output<I, S>(&self, args: I) -> Result<std::process::Output>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut command = deploy_ssh_command();
        command.arg(&self.host).args(args);
        command
            .output()
            .with_context(|| format!("failed to run ssh {}", self.host))
    }

    fn ssh_output_with_password<I, S>(
        &self,
        args: I,
        password: &SudoPassword,
    ) -> Result<std::process::Output>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut command = deploy_ssh_command();
        command
            .arg(&self.host)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command
            .spawn()
            .with_context(|| format!("failed to run ssh {}", self.host))?;
        let mut stdin = child
            .stdin
            .take()
            .context("sudo validation child stdin was not available")?;
        password.write_with_newline(&mut stdin)?;
        drop(stdin);
        child
            .wait_with_output()
            .with_context(|| format!("failed to wait for ssh {}", self.host))
    }

    fn ssh_status<I, S>(&self, args: I) -> Result<()>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let output = self.ssh_output(args)?;
        if output.status.success() {
            return Ok(());
        }
        bail!(
            "ssh {} failed with status {}\nstdout:\n{}\nstderr:\n{}",
            self.host,
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    }

    fn ssh_stdout<I, S>(&self, args: I) -> Result<String>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let output = self.ssh_output(args)?;
        if output.status.success() {
            return String::from_utf8(output.stdout)
                .with_context(|| format!("ssh {} wrote non-UTF8 stdout", self.host));
        }
        bail!(
            "ssh {} failed with status {}\nstdout:\n{}\nstderr:\n{}",
            self.host,
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    }

    fn rsync<I, S>(&self, args: I) -> Result<()>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut command = deploy_command("rsync");
        command.args(args);
        let status = self.ui.command_status(&mut command)?;
        if status.success() {
            Ok(())
        } else {
            bail!("rsync failed with status {status}")
        }
    }

    fn run_helper(&self, args: &[&str]) -> Result<()> {
        let mut command = vec!["sudo", HELPER_PATH];
        command.extend_from_slice(args);
        self.ssh_status(command)
    }

    fn github_release_reachable_from_robot(&self, url: &str) -> Result<bool> {
        let mut command = deploy_ssh_command();
        command
            .arg(&self.host)
            .arg("url=$(cat); curl --head --fail --location --silent --show-error --connect-timeout 5 --max-time 15 \"$url\" >/dev/null")
            .stdin(Stdio::piped());
        let mut child = command.spawn().with_context(|| {
            format!("failed to start GitHub reachability probe on {}", self.host)
        })?;
        child
            .stdin
            .take()
            .context("reachability probe stdin was unavailable")?
            .write_all(url.as_bytes())?;
        Ok(child.wait()?.success())
    }

    fn download_artifact(&self, generation: &str, artifact: &DownloadArtifact) -> Result<()> {
        let size = artifact.size.to_string();
        let mut command = deploy_ssh_command();
        command
            .arg(&self.host)
            .args([
                "sudo",
                HELPER_PATH,
                "download-artifact",
                generation,
                size.as_str(),
                artifact.sha256.as_str(),
                artifact.archive_binary_name.as_str(),
                artifact.install_binary_name.as_str(),
            ])
            .stdin(Stdio::piped());
        let mut child = command
            .spawn()
            .with_context(|| format!("failed to start robot download for {}", artifact.package))?;
        child
            .stdin
            .take()
            .context("robot download stdin was unavailable")?
            .write_all(artifact.url.as_bytes())?;
        let status = child.wait()?;
        if status.success() {
            Ok(())
        } else {
            bail!(
                "robot download failed for {} {} with status {status}",
                artifact.package,
                artifact.version
            )
        }
    }

    /// `sudo -n true` tests blanket sudo, but the group-model grant needs
    /// all three of: the helper installed and executable, this user enrolled
    /// in the `phoxal-deploy` group, and the sudoers fragment actually
    /// authorizing the call for that group - so probe the grant itself
    /// rather than infer it from blanket sudo. Running the installed helper
    /// with no arguments hits its unknown-verb branch and exits 64, so exit
    /// 0 or 64 proves sudo authorized this user for the helper; a sudo
    /// password failure exits 1 without ever running the helper.
    fn probe_helper_grant(&self) -> bool {
        let helper_installed = self
            .ssh_output(["test", "-x", HELPER_PATH])
            .map(|output| output.status.success())
            .unwrap_or(false);
        if !helper_installed {
            return false;
        }
        if !self.probe_phoxal_deploy_group_membership() {
            return false;
        }
        self.ssh_output(["sudo", "-n", HELPER_PATH])
            .map(|output| matches!(output.status.code(), Some(0) | Some(64)))
            .unwrap_or(false)
    }

    /// Exact-word match against `id -nG` output - a substring match would
    /// wrongly accept e.g. a group named `phoxal-deploy-readonly`.
    fn probe_phoxal_deploy_group_membership(&self) -> bool {
        self.ssh_stdout(["id", "-nG"])
            .map(|output| {
                output
                    .split_whitespace()
                    .any(|group| group == "phoxal-deploy")
            })
            .unwrap_or(false)
    }

    fn probe_helper_stale(&self) -> bool {
        let expected = helper_script_sha256();
        match self.ssh_stdout(["sha256sum", HELPER_PATH]) {
            Ok(output) => output.split_whitespace().next() != Some(expected.as_str()),
            Err(_) => true,
        }
    }
}

trait PayloadSyncRemote {
    fn remote_host(&self) -> &str;

    fn run_ssh_status<I, S>(&mut self, args: I) -> Result<()>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>;

    fn run_rsync<I, S>(&mut self, args: I) -> Result<()>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>;

    fn run_helper(&mut self, args: &[&str]) -> Result<()>;
}

impl PayloadSyncRemote for SshTransport {
    fn remote_host(&self) -> &str {
        &self.host
    }

    fn run_ssh_status<I, S>(&mut self, args: I) -> Result<()>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        SshTransport::ssh_status(self, args)
    }

    fn run_rsync<I, S>(&mut self, args: I) -> Result<()>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        SshTransport::rsync(self, args)
    }

    fn run_helper(&mut self, args: &[&str]) -> Result<()> {
        SshTransport::run_helper(self, args)
    }
}

fn remote_staging_dir(prefix: &str) -> String {
    format!("{prefix}{}", std::process::id())
}

fn sync_payload_via_helper<R>(
    remote: &mut R,
    payload: &RenderedPayload,
    remote_tmp: &str,
) -> Result<()>
where
    R: PayloadSyncRemote,
{
    remote.run_ssh_status(["rm", "-rf", remote_tmp])?;
    let install_result = (|| -> Result<()> {
        remote.run_ssh_status(["mkdir", "-p", remote_tmp])?;
        let remote_dest = OsString::from(format!("{}:{remote_tmp}/", remote.remote_host()));
        remote.run_rsync(vec![
            OsString::from("-az"),
            OsString::from("--delete"),
            payload_opt(payload.root.path()).join("").into_os_string(),
            remote_dest,
        ])?;
        remote.run_helper(&[
            "prepare-release",
            remote_tmp,
            &payload.install_plan.release_generation,
        ])?;
        Ok(())
    })();
    let _ = remote.run_ssh_status(["rm", "-rf", remote_tmp]);
    install_result?;
    sync_identity_files(remote, payload)
}

fn sync_identity_files<R>(remote: &mut R, payload: &RenderedPayload) -> Result<()>
where
    R: PayloadSyncRemote,
{
    if !payload.install_plan.identity_files.is_empty() {
        remote.run_ssh_status(["install", "-d", "-m", "0700", IDENTITY_DIR])?;
    }
    for identity in &payload.install_plan.identity_files {
        let remote_dest =
            OsString::from(format!("{}:{}", remote.remote_host(), identity.remote_path));
        remote.run_rsync(vec![
            OsString::from("-az"),
            identity.local_path.clone().into_os_string(),
            remote_dest,
        ])?;
        remote.run_ssh_status(["chmod", "0600", identity.remote_path.as_str()])?;
    }
    Ok(())
}

impl DeployTransport for SshTransport {
    fn probe(&mut self) -> Result<RemoteProbe> {
        let arch = self.ssh_stdout(["uname", "-m"])?.trim().to_string();
        let bootstrap_required = self
            .ssh_output(["test", "-d", OPT_ROOT])
            .map(|output| !output.status.success())
            .unwrap_or(true);
        let remote_user = self.ssh_stdout(["whoami"])?.trim().to_string();
        let sudo_noninteractive = self
            .ssh_output(["sudo", "-n", "true"])
            .map(|output| output.status.success())
            .unwrap_or(false);
        let helper_stale = self.probe_helper_stale();
        // Blanket passwordless sudo must NOT short-circuit this to true: a
        // device with blanket sudo but no `phoxal-deploy` membership (e.g.
        // bootstrapped under the old per-user model) needs the bootstrap
        // repair path to run so it converges to the group model - which it
        // can do non-interactively here since blanket sudo covers it.
        let helper_grant = self.probe_helper_grant();
        Ok(RemoteProbe {
            arch,
            bootstrap_required,
            remote_user,
            sudo_noninteractive,
            helper_grant,
            helper_stale,
        })
    }

    fn validate_sudo_password(&mut self, password: &SudoPassword) -> Result<bool> {
        self.ssh_output_with_password(sudo_validate_args(), password)
            .map(|output| output.status.success())
    }

    fn bootstrap(
        &mut self,
        helper: &BootstrapScripts,
        sudo_password: Option<&SudoPassword>,
    ) -> Result<()> {
        let script = bootstrap_script(helper);
        let remote_path = format!("/tmp/phoxal-bootstrap.{}.sh", std::process::id());

        // Transfer the script over a plain (non-sudo) ssh first. The sudo
        // password, when needed, is reserved for the script execution stdin.
        let mut upload_command = deploy_ssh_command();
        upload_command
            .arg(&self.host)
            .arg(format!("cat > {remote_path}"))
            .stdin(Stdio::piped());
        let mut upload = upload_command
            .spawn()
            .with_context(|| format!("failed to start bootstrap upload ssh {}", self.host))?;
        let mut upload_stdin = upload
            .stdin
            .take()
            .context("bootstrap upload child stdin was not available")?;
        upload_stdin
            .write_all(script.as_bytes())
            .context("failed to write bootstrap script")?;
        drop(upload_stdin);
        let upload_status = upload
            .wait()
            .context("failed to wait for bootstrap upload ssh")?;
        if !upload_status.success() {
            bail!(
                "failed to upload bootstrap script to {}: status {upload_status}",
                self.host
            );
        }

        let mut run = deploy_ssh_command();
        run.arg(&self.host).args(sudo_bootstrap_args(&remote_path));
        if sudo_password.is_some() {
            run.stdin(Stdio::piped());
        } else {
            run.stdin(Stdio::null());
        }
        let mut child = self
            .ui
            .command_spawn(&mut run)
            .with_context(|| format!("failed to run bootstrap script on {}", self.host))?;
        if let Some(password) = sudo_password {
            let mut stdin = child
                .stdin
                .take()
                .context("bootstrap child stdin was not available")?;
            password.write_with_newline(&mut stdin)?;
            drop(stdin);
        }
        let run_status = child
            .wait()
            .with_context(|| format!("failed to wait for bootstrap script on {}", self.host))?;

        // Best-effort cleanup: report bootstrap's own failure first, since
        // that's the actionable error; a stray temp file is not.
        let _ = self.ssh_status(["rm", "-f", &remote_path]);

        if run_status.success() {
            Ok(())
        } else {
            bail!("remote bootstrap failed with status {run_status}")
        }
    }

    fn list_installed_units(&mut self) -> Result<Vec<String>> {
        let output = self.ssh_output([
            "systemctl",
            "list-unit-files",
            "phoxal*",
            "--no-legend",
            "--no-pager",
        ])?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        // systemctl list-unit-files exits 1 with empty output when the pattern
        // matches nothing - the normal state of a freshly bootstrapped host.
        if !output.status.success() && (!stdout.trim().is_empty() || !stderr.trim().is_empty()) {
            bail!(
                "failed to list installed phoxal units: ssh {} failed with status {}\nstdout:\n{stdout}\nstderr:\n{stderr}",
                self.host,
                output.status
            );
        }
        Ok(stdout
            .lines()
            .filter_map(|line| line.split_whitespace().next().map(str::to_string))
            .filter(|unit| managed_unit_name(unit))
            .collect())
    }

    fn github_release_reachable(&mut self, url: &str) -> Result<bool> {
        self.github_release_reachable_from_robot(url)
    }

    fn prepare_host_transfer_fallback(
        &mut self,
        payload: &mut RenderedPayload,
        ui: &crate::Ui,
    ) -> Result<()> {
        stage_official_fallback(payload, ui)
    }

    fn sync_payload(&mut self, payload: &RenderedPayload) -> Result<()> {
        let remote_tmp = remote_staging_dir(PAYLOAD_STAGING_PREFIX);
        sync_payload_via_helper(self, payload, &remote_tmp)
    }

    fn download_official_artifacts(
        &mut self,
        generation: &str,
        artifacts: &[DownloadArtifact],
    ) -> Result<()> {
        let transport = self.clone();
        run_bounded(artifacts, DOWNLOAD_CONCURRENCY, |artifact| {
            transport.download_artifact(generation, artifact)
        })
    }

    fn install_units(&mut self, payload: &RenderedPayload, stale_units: &[String]) -> Result<()> {
        let _ = stale_units;
        for unit in &payload.unit_names {
            self.run_helper(&["install-unit", unit])?;
        }
        self.pending_units = payload.unit_names.clone();
        Ok(())
    }

    fn activate_release(&mut self, generation: &str) -> Result<()> {
        self.run_helper(&["activate-release", generation])
    }

    fn rollback_release(&mut self) -> Result<()> {
        self.run_helper(&["rollback-release"])
    }

    fn finalize_units(&mut self, stale_units: &[String]) -> Result<()> {
        for unit in stale_units {
            self.run_helper(&["disable-unit", unit])?;
            self.run_helper(&["remove-unit", unit])?;
        }
        self.run_helper(&["daemon-reload"])
    }

    fn restart(&mut self) -> Result<()> {
        self.run_helper(&["daemon-reload"])?;
        for unit in self.pending_units.clone() {
            self.run_helper(&["enable-unit", &unit])?;
        }
        self.run_helper(&["restart-target"])
    }

    fn health_report(&mut self, units: &[String], deadline: Duration) -> Result<HealthReport> {
        let start = std::time::Instant::now();
        loop {
            let report = self.collect_health(units)?;
            if report.is_ok() || start.elapsed() >= deadline {
                return Ok(report);
            }
            std::thread::sleep(Duration::from_secs(1));
        }
    }
}

impl SshTransport {
    fn collect_health(&self, units: &[String]) -> Result<HealthReport> {
        let mut reports = Vec::new();
        for unit in units {
            let active = self
                .ssh_output(["systemctl", "is-active", unit])
                .map(|output| output.status.success())
                .unwrap_or(false);
            let show = self
                .ssh_stdout([
                    "systemctl",
                    "show",
                    unit,
                    "-p",
                    "ActiveState",
                    "-p",
                    "SubState",
                ])
                .unwrap_or_default();
            let fields = show
                .lines()
                .filter_map(|line| line.split_once('='))
                .map(|(key, value)| (key.to_string(), value.to_string()))
                .collect::<BTreeMap<_, _>>();
            let journal_excerpt = if active {
                Vec::new()
            } else {
                self.ssh_stdout([
                    "journalctl",
                    "-u",
                    unit,
                    "-n",
                    "20",
                    "--no-pager",
                    "--output",
                    "cat",
                ])
                .unwrap_or_default()
                .lines()
                .map(str::to_string)
                .collect()
            };
            reports.push(HealthUnitReport {
                unit: unit.clone(),
                participant: participant_from_unit(unit),
                ready: active,
                active_state: fields
                    .get("ActiveState")
                    .cloned()
                    .unwrap_or_else(|| if active { "active" } else { "unknown" }.to_string()),
                sub_state: fields.get("SubState").cloned().unwrap_or_default(),
                journal_excerpt,
            });
        }
        Ok(HealthReport { units: reports })
    }
}

fn participant_from_unit(unit: &str) -> Option<String> {
    unit.strip_prefix("phoxal-participant-")
        .and_then(|rest| rest.strip_suffix(".service"))
        .map(str::to_string)
        .or_else(|| {
            (unit == "phoxal-router.service").then(|| SITE_INFRASTRUCTURE_ROUTER.to_string())
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    use crate::resolver::ResolvedComponent;
    use clap::Parser;
    use phoxal_cli_test_support::write_basic_project;

    mod phoxal_cli_test_support {
        use super::*;
        use crate::catalog::{SelectionChannel as CatalogChannel, fixture_tool_entry_for_tests};

        pub fn write_basic_project(root: &Path) -> Result<()> {
            write_fixture_catalog(root)?;
            fs::write(root.join("robot.yaml"), basic_robot_yaml())?;
            fs::write(root.join("robot.dev.yaml"), basic_robot_dev_overlay_yaml())?;
            write_robot_structure(root)?;
            write_service_crate(root, "navtask", "service", "navtask")?;
            write_component_metadata(root, "ddsm115")?;
            Ok(())
        }

        pub fn write_driver_project(root: &Path) -> Result<()> {
            write_fixture_catalog(root)?;
            fs::write(root.join("robot.yaml"), driver_robot_yaml())?;
            fs::write(root.join("robot.dev.yaml"), driver_robot_dev_overlay_yaml())?;
            write_robot_structure(root)?;
            write_service_crate(root, "navtask", "service", "navtask")?;
            write_driver_crate(root, "ddsm115", "driver-ddsm115")?;
            write_component_metadata(root, "ddsm115")?;
            Ok(())
        }

        pub fn write_bench_camera_project(root: &Path) -> Result<()> {
            write_fixture_catalog(root)?;
            fs::write(root.join("robot.yaml"), bench_camera_robot_yaml())?;
            fs::write(
                root.join("robot.dev.yaml"),
                bench_camera_robot_dev_overlay_yaml(),
            )?;
            write_robot_structure(root)?;
            write_component_metadata(root, "bench_camera")?;
            let component_dir = root.join("components").join("bench_camera");
            fs::create_dir_all(component_dir.join("src"))?;
            fs::create_dir_all(component_dir.join("target/debug"))?;
            fs::write(
                component_dir.join("Cargo.toml"),
                "[package]\nname = \"bench-camera\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
            )?;
            fs::write(component_dir.join("src/main.rs"), "fn main() {}\n")?;
            fs::write(component_dir.join("target/debug/ignored"), "ignored\n")?;
            Ok(())
        }

        pub fn write_catalog_only_project(root: &Path) -> Result<()> {
            write_fixture_catalog(root)?;
            fs::write(root.join("robot.yaml"), catalog_only_robot_yaml())?;
            write_robot_structure(root)?;
            Ok(())
        }

        pub fn write_native_dep_project(root: &Path) -> Result<()> {
            write_fixture_catalog(root)?;
            fs::write(root.join("robot.yaml"), basic_robot_yaml())?;
            fs::write(root.join("robot.dev.yaml"), basic_robot_dev_overlay_yaml())?;
            let dir = root.join("runtimes/navtask");
            fs::create_dir_all(dir.join("src"))?;
            fs::write(
                dir.join("Cargo.toml"),
                "[package]\nname = \"navtask\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\nopencv = \"0.1\"\n",
            )?;
            fs::write(dir.join("src/main.rs"), service_main("service", "navtask"))?;
            Ok(())
        }

        fn write_fixture_catalog(root: &Path) -> Result<()> {
            let catalog = crate::catalog::fixture_catalog_for_tests(vec![
                // A decoy extra catalog entry proving multi-entry catalogs
                // resolve fine; kept off the `stable` channel every fixture
                // robot below targets, so it never becomes a real deploy
                // participant (the lean manifest schema has no separate
                // "declared target" concept to keep it inert by target alone
                // - see `resolver::select_latest_artifact_entries`).
                crate::catalog::fixture_service_entry_for_tests(
                    "fixture_only",
                    "0.1.0",
                    crate::catalog::SelectionChannel::Nightly,
                    "test-only-target",
                    false,
                    vec![crate::catalog::fixture_contract_for_tests(
                        "v1::fixture::Only",
                        "publish",
                    )],
                ),
                fixture_tool_entry_for_tests(
                    "router",
                    "0.1.0",
                    CatalogChannel::Stable,
                    "aarch64-unknown-linux-gnu",
                    true,
                    Vec::new(),
                ),
                fixture_tool_entry_for_tests(
                    "router",
                    "0.1.0",
                    CatalogChannel::Stable,
                    &crate::resolver::host_target_triple(),
                    true,
                    Vec::new(),
                ),
            ]);
            fs::write(
                root.join("catalog.json"),
                serde_json::to_string_pretty(&catalog)?,
            )?;
            Ok(())
        }

        fn write_service_crate(
            root: &Path,
            name: &str,
            kind: &str,
            artifact_id: &str,
        ) -> Result<()> {
            let dir = root.join("runtimes").join(name);
            fs::create_dir_all(dir.join("src"))?;
            fs::write(
                dir.join("Cargo.toml"),
                format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n"),
            )?;
            fs::write(dir.join("src/main.rs"), service_main(kind, artifact_id))?;
            Ok(())
        }

        fn write_driver_crate(root: &Path, name: &str, package: &str) -> Result<()> {
            let dir = root.join("components").join(name);
            fs::create_dir_all(dir.join("src"))?;
            fs::write(
                dir.join("Cargo.toml"),
                format!(
                    "[package]\nname = \"{package}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n"
                ),
            )?;
            fs::write(dir.join("src/main.rs"), service_main("driver", name))?;
            Ok(())
        }

        fn write_robot_structure(root: &Path) -> Result<()> {
            fs::write(root.join("structure.urdf"), robot_structure_urdf())?;
            Ok(())
        }

        fn write_component_metadata(root: &Path, name: &str) -> Result<()> {
            let dir = root.join("components").join(name);
            fs::create_dir_all(&dir)?;
            fs::write(dir.join("component.yaml"), component_yaml())?;
            fs::write(dir.join("structure.urdf"), component_structure_urdf(name))?;
            Ok(())
        }

        fn service_main(kind: &str, artifact_id: &str) -> String {
            format!(
                "fn main() {{\n    if std::env::args().nth(1).as_deref() == Some(\"emit-apis\") {{\n        println!(\"{{}}\", r#\"{{\"artifact\":{{\"kind\":\"{kind}\",\"id\":\"{artifact_id}\"}},\"participant_class\":\"checked\",\"api_version\":\"source\",\"required_contracts\":[]}}\"#);\n    }}\n}}\n"
            )
        }

        fn basic_robot_yaml() -> &'static str {
            r#"schema: robot/v0
robot:
  id: testbot
  namespace: dev
  motion_limits:
    max_linear_speed_mps: 0.6
    max_angular_speed_radps: 2.0
  structure: structure.urdf
  kinematic:
    kind: differential
    left_actuators: [left_drive.motor]
    right_actuators: [right_drive.motor]
    left_encoders: [left_drive.encoder]
    right_encoders: [right_drive.encoder]
    wheel_radius_m: 0.1
    wheel_base_m: 0.5
  components:
    left_drive:
      component: ddsm115
      mount_link: left_wheel
    right_drive:
      component: ddsm115
      mount_link: right_wheel
artifacts:
  channel: stable
  catalog: catalog.json
services:
  navtask:
    path: runtimes/navtask
"#
        }

        /// Path pins are dev-overlay-only; every fixture project pairs its base
        /// `robot.yaml` with this `robot.dev.yaml` overlay (loaded via
        /// `--env dev`, see `dry_options`/`live_options`) so local component
        /// asset/driver directories resolve without a real catalog/network.
        fn basic_robot_dev_overlay_yaml() -> &'static str {
            r#"artifacts:
  pins:
    phoxal/component-ddsm115:
      path: components/ddsm115
"#
        }

        fn bench_camera_robot_yaml() -> &'static str {
            r#"schema: robot/v0
robot:
  id: benchbot
  namespace: dev
  motion_limits:
    max_linear_speed_mps: 0.6
    max_angular_speed_radps: 2.0
  structure: structure.urdf
  kinematic:
    kind: omnidirectional
    actuators: [front_camera.motor]
    encoders: [front_camera.encoder]
  components:
    front_camera:
      component: bench_camera
      mount_link: camera_mount
artifacts:
  channel: stable
  catalog: catalog.json
"#
        }

        fn bench_camera_robot_dev_overlay_yaml() -> &'static str {
            r#"artifacts:
  pins:
    phoxal/component-bench_camera:
      path: components/bench_camera
"#
        }

        fn catalog_only_robot_yaml() -> &'static str {
            r#"schema: robot/v0
robot:
  id: catalogbot
  namespace: dev
  motion_limits:
    max_linear_speed_mps: 0.6
    max_angular_speed_radps: 2.0
  structure: structure.urdf
  kinematic:
    kind: omnidirectional
    actuators: [catalog_drive.motor]
    encoders: [catalog_drive.encoder]
  components:
    catalog_drive:
      component: catalog_motor
      mount_link: left_wheel
artifacts:
  channel: stable
  catalog: catalog.json
  pins:
    phoxal/component-catalog_motor:
      git: /definitely/not/a/component-assets-repo
      rev: main
"#
        }

        fn driver_robot_yaml() -> &'static str {
            r#"schema: robot/v0
robot:
  id: testbot
  namespace: dev
  motion_limits:
    max_linear_speed_mps: 0.6
    max_angular_speed_radps: 2.0
  structure: structure.urdf
  kinematic:
    kind: differential
    left_actuators: [left_drive.motor]
    right_actuators: [right_drive.motor]
    left_encoders: [left_drive.encoder]
    right_encoders: [right_drive.encoder]
    wheel_radius_m: 0.1
    wheel_base_m: 0.5
  components:
    left_drive:
      component: ddsm115
      mount_link: left_wheel
      driver:
        connection: { type: serial, port: /dev/ttyUSB0, baud: 115200 }
    right_drive:
      component: ddsm115
      mount_link: right_wheel
      driver:
        connection: { type: i2c, bus: 1, address: 16 }
artifacts:
  channel: stable
  catalog: catalog.json
services:
  navtask:
    path: runtimes/navtask
"#
        }

        fn driver_robot_dev_overlay_yaml() -> &'static str {
            r#"artifacts:
  pins:
    phoxal/component-ddsm115:
      path: components/ddsm115
"#
        }

        fn robot_structure_urdf() -> &'static str {
            r#"<robot name="testbot">
  <link name="base_footprint" />
  <link name="base_link" />
  <link name="left_wheel" />
  <link name="right_wheel" />
  <link name="camera_mount" />
  <joint name="root" type="fixed">
    <parent link="base_footprint" />
    <child link="base_link" />
  </joint>
  <joint name="left_mount" type="fixed">
    <parent link="base_link" />
    <child link="left_wheel" />
  </joint>
  <joint name="right_mount" type="fixed">
    <parent link="base_link" />
    <child link="right_wheel" />
  </joint>
  <joint name="camera_mount_joint" type="fixed">
    <parent link="base_link" />
    <child link="camera_mount" />
  </joint>
</robot>
"#
        }

        fn component_yaml() -> &'static str {
            r#"schema: component/v0
structure: structure.urdf
capabilities:
  motor:
    kind: motor
    target: { kind: joint, id: wheel_joint }
    command: velocity
    gear_ratio: 1.0
  encoder:
    kind: encoder
    target: { kind: joint, id: wheel_joint }
    publish_rate_hz: 50.0
    gear_ratio: 1.0
  rgb:
    kind: camera
    target: { kind: link, id: camera_link }
    mode: rgb
    publish_rate_hz: 30.0
    width_px: 640
    height_px: 480
"#
        }

        fn component_structure_urdf(name: &str) -> String {
            format!(
                r#"<robot name="{name}">
  <link name="camera_link" />
  <link name="wheel_link" />
  <joint name="wheel_joint" type="continuous">
    <parent link="camera_link" />
    <child link="wheel_link" />
  </joint>
</robot>
"#
            )
        }
    }

    #[derive(Debug)]
    struct FakeTransport {
        probe: RemoteProbe,
        installed_units: Vec<String>,
        health: HealthReport,
        bootstrapped: bool,
        bootstrap_fragment_seen: Option<String>,
        bootstrap_remote_user_seen: Option<String>,
        validation_results: VecDeque<bool>,
        validation_password_stdin: Vec<Vec<u8>>,
        bootstrap_sudo_command_seen: Option<Vec<String>>,
        bootstrap_password_stdin: Vec<Vec<u8>>,
        synced: bool,
        stale_removed: Vec<String>,
        restarted: bool,
        github_reachable: bool,
        downloaded: Vec<DownloadArtifact>,
        activated: Option<String>,
        rolled_back: bool,
        finalized: bool,
        fallback_prepared: bool,
        download_fails: bool,
        active_generation: Option<String>,
        previous_generation: Option<String>,
    }

    impl FakeTransport {
        fn healthy() -> Self {
            Self {
                probe: RemoteProbe {
                    arch: "aarch64".to_string(),
                    bootstrap_required: true,
                    remote_user: "robot".to_string(),
                    sudo_noninteractive: true,
                    helper_grant: true,
                    helper_stale: false,
                },
                installed_units: Vec::new(),
                health: HealthReport { units: Vec::new() },
                bootstrapped: false,
                bootstrap_fragment_seen: None,
                bootstrap_remote_user_seen: None,
                validation_results: VecDeque::from([true]),
                validation_password_stdin: Vec::new(),
                bootstrap_sudo_command_seen: None,
                bootstrap_password_stdin: Vec::new(),
                synced: false,
                stale_removed: Vec::new(),
                restarted: false,
                github_reachable: true,
                downloaded: Vec::new(),
                activated: None,
                rolled_back: false,
                finalized: false,
                fallback_prepared: false,
                download_fails: false,
                active_generation: Some("releases/previous-generation".to_string()),
                previous_generation: None,
            }
        }
    }

    impl DeployTransport for FakeTransport {
        fn probe(&mut self) -> Result<RemoteProbe> {
            Ok(self.probe.clone())
        }

        fn validate_sudo_password(&mut self, password: &SudoPassword) -> Result<bool> {
            let mut stdin = Vec::new();
            password.write_with_newline(&mut stdin)?;
            self.validation_password_stdin.push(stdin);
            Ok(self.validation_results.pop_front().unwrap_or(true))
        }

        fn bootstrap(
            &mut self,
            helper: &BootstrapScripts,
            sudo_password: Option<&SudoPassword>,
        ) -> Result<()> {
            self.bootstrapped = true;
            self.bootstrap_fragment_seen = Some(helper.sudoers_fragment.clone());
            self.bootstrap_remote_user_seen = Some(helper.remote_user.clone());
            self.bootstrap_sudo_command_seen = Some(args_to_strings(sudo_bootstrap_args(
                "/tmp/phoxal-bootstrap.TEST.sh",
            )));
            if let Some(password) = sudo_password {
                let mut stdin = Vec::new();
                password.write_with_newline(&mut stdin)?;
                self.bootstrap_password_stdin.push(stdin);
            }
            Ok(())
        }

        fn list_installed_units(&mut self) -> Result<Vec<String>> {
            Ok(self.installed_units.clone())
        }

        fn github_release_reachable(&mut self, _url: &str) -> Result<bool> {
            Ok(self.github_reachable)
        }

        fn prepare_host_transfer_fallback(
            &mut self,
            _payload: &mut RenderedPayload,
            _ui: &crate::Ui,
        ) -> Result<()> {
            self.fallback_prepared = true;
            Ok(())
        }

        fn sync_payload(&mut self, _payload: &RenderedPayload) -> Result<()> {
            self.synced = true;
            Ok(())
        }

        fn download_official_artifacts(
            &mut self,
            _generation: &str,
            artifacts: &[DownloadArtifact],
        ) -> Result<()> {
            if self.download_fails {
                bail!("simulated verification failure");
            }
            self.downloaded = artifacts.to_vec();
            Ok(())
        }

        fn install_units(
            &mut self,
            _payload: &RenderedPayload,
            stale_units: &[String],
        ) -> Result<()> {
            self.stale_removed = stale_units.to_vec();
            Ok(())
        }

        fn activate_release(&mut self, generation: &str) -> Result<()> {
            self.previous_generation = self.active_generation.take();
            self.active_generation = Some(format!("releases/{generation}"));
            self.activated = Some(generation.to_string());
            Ok(())
        }

        fn rollback_release(&mut self) -> Result<()> {
            std::mem::swap(&mut self.active_generation, &mut self.previous_generation);
            self.rolled_back = true;
            Ok(())
        }

        fn finalize_units(&mut self, stale_units: &[String]) -> Result<()> {
            self.stale_removed = stale_units.to_vec();
            self.finalized = true;
            Ok(())
        }

        fn restart(&mut self) -> Result<()> {
            self.restarted = true;
            Ok(())
        }

        fn health_report(&mut self, units: &[String], _deadline: Duration) -> Result<HealthReport> {
            if self.health.units.is_empty() {
                Ok(HealthReport {
                    units: units
                        .iter()
                        .map(|unit| HealthUnitReport {
                            unit: unit.clone(),
                            participant: participant_from_unit(unit),
                            ready: true,
                            active_state: "active".to_string(),
                            sub_state: "running".to_string(),
                            journal_excerpt: Vec::new(),
                        })
                        .collect(),
                })
            } else {
                Ok(self.health.clone())
            }
        }
    }

    #[derive(Debug)]
    struct ScriptedSudoPasswordSource {
        env_password: Option<Vec<u8>>,
        prompt_passwords: VecDeque<Vec<u8>>,
        env_calls: usize,
        prompt_calls: usize,
        prompts_seen: Vec<String>,
    }

    impl ScriptedSudoPasswordSource {
        fn none() -> Self {
            Self {
                env_password: None,
                prompt_passwords: VecDeque::new(),
                env_calls: 0,
                prompt_calls: 0,
                prompts_seen: Vec::new(),
            }
        }

        fn with_env(password: &str) -> Self {
            let mut source = Self::none();
            source.env_password = Some(password.as_bytes().to_vec());
            source
        }

        fn with_prompts(passwords: &[&str]) -> Self {
            let mut source = Self::none();
            source.prompt_passwords = passwords
                .iter()
                .map(|password| password.as_bytes().to_vec())
                .collect();
            source
        }
    }

    impl SudoPasswordSource for ScriptedSudoPasswordSource {
        fn password_from_env(&mut self) -> Option<SudoPassword> {
            self.env_calls += 1;
            self.env_password.take().map(SudoPassword::new)
        }

        fn read_password(&mut self, prompt: &str) -> Result<SudoPassword> {
            self.prompt_calls += 1;
            self.prompts_seen.push(prompt.to_string());
            self.prompt_passwords
                .pop_front()
                .map(SudoPassword::new)
                .context("scripted sudo password source was exhausted")
        }
    }

    #[derive(Debug)]
    struct FakePayloadRemote {
        host: String,
        ssh_statuses: Vec<Vec<String>>,
        rsyncs: Vec<Vec<String>>,
        helper_calls: Vec<Vec<String>>,
    }

    impl FakePayloadRemote {
        fn new(host: &str) -> Self {
            Self {
                host: host.to_string(),
                ssh_statuses: Vec::new(),
                rsyncs: Vec::new(),
                helper_calls: Vec::new(),
            }
        }
    }

    impl PayloadSyncRemote for FakePayloadRemote {
        fn remote_host(&self) -> &str {
            &self.host
        }

        fn run_ssh_status<I, S>(&mut self, args: I) -> Result<()>
        where
            I: IntoIterator<Item = S>,
            S: AsRef<OsStr>,
        {
            self.ssh_statuses.push(args_to_strings(args));
            Ok(())
        }

        fn run_rsync<I, S>(&mut self, args: I) -> Result<()>
        where
            I: IntoIterator<Item = S>,
            S: AsRef<OsStr>,
        {
            self.rsyncs.push(args_to_strings(args));
            Ok(())
        }

        fn run_helper(&mut self, args: &[&str]) -> Result<()> {
            self.helper_calls
                .push(args.iter().map(|arg| (*arg).to_string()).collect());
            Ok(())
        }
    }

    fn args_to_strings<I, S>(args: I) -> Vec<String>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        args.into_iter()
            .map(|arg| arg.as_ref().to_string_lossy().into_owned())
            .collect()
    }

    fn payload_relative_files(root: &Path) -> Result<Vec<String>> {
        let opt = payload_opt(root);
        let mut files = Vec::new();
        collect_relative_files(&opt, &opt, &mut files)?;
        files.sort();
        Ok(files)
    }

    fn collect_relative_files(base: &Path, dir: &Path, files: &mut Vec<String>) -> Result<()> {
        for entry in
            fs::read_dir(dir).with_context(|| format!("failed to read {}", dir.display()))?
        {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                collect_relative_files(base, &path, files)?;
            } else if path.is_file() {
                files.push(path.strip_prefix(base)?.to_string_lossy().into_owned());
            }
        }
        Ok(())
    }

    /// Every `phoxal_cli_test_support` fixture stages its component asset/driver
    /// path pins in a `robot.dev.yaml` overlay (path pins are dev-overlay-only
    /// in the new grammar); both option builders load it so fixture projects
    /// resolve their components without touching a real catalog/network.
    fn dry_options() -> DeployOptions {
        DeployOptions {
            host: None,
            dry_run: true,
            target: Some("aarch64".to_string()),
            overlays: vec!["dev".to_string()],
            catalog_source: None,
            health_timeout: Duration::from_secs(3),
        }
    }

    fn live_options() -> DeployOptions {
        DeployOptions {
            host: Some("robot@test".to_string()),
            dry_run: false,
            target: None,
            overlays: vec!["dev".to_string()],
            catalog_source: None,
            health_timeout: Duration::from_secs(3),
        }
    }

    #[test]
    fn parses_single_deploy_verb_and_rejects_build_pair() {
        let cli = crate::commands::Cli::try_parse_from([
            "phoxal-cli",
            "deploy",
            "--dry-run",
            "--target",
            "aarch64",
        ])
        .expect("deploy dry-run parses");
        let crate::commands::RootCommand::Deploy(command) = cli.command else {
            panic!("expected deploy command");
        };
        assert!(command.dry_run);
        assert_eq!(command.target.as_deref(), Some("aarch64"));

        assert!(crate::commands::Cli::try_parse_from(["phoxal-cli", "deploy", "build"]).is_err());
        assert!(
            crate::commands::Cli::try_parse_from([
                "phoxal-cli",
                "deploy",
                "--dry-run",
                "--target",
                "compose",
            ])
            .is_ok(),
            "clap accepts the value so deploy can emit the designed diagnostic"
        );
    }

    #[test]
    fn target_parser_reserves_update_targets_and_blocks_compose_balena() {
        assert!(target_from_selector("mender").is_err());
        assert!(target_from_selector("rauc").is_err());
        assert!(target_from_selector("compose").is_err());
        assert!(target_from_selector("balena").is_err());
        assert_eq!(
            target_from_selector("aarch64").unwrap().local_triple,
            "aarch64-unknown-linux-musl"
        );
    }

    #[test]
    fn dry_run_renders_units_env_release_and_install_plan() -> Result<()> {
        let _phoxal_home = ScratchPhoxalHome::new()?;
        let temp = tempfile::tempdir()?;
        write_basic_project(temp.path())?;
        let payload = prepare_deploy(
            temp.path(),
            &dry_options(),
            target_for_arch("aarch64"),
            false,
            DRY_RUN_REMOTE_USER,
            &crate::Ui::from_env(),
        )?;
        assert!(
            payload
                .rendered_units
                .contains_key("/etc/systemd/system/phoxal.target")
        );
        assert!(
            payload
                .rendered_units
                .contains_key("/etc/systemd/system/phoxal-router.service")
        );
        let participant_unit = payload
            .rendered_units
            .get("/etc/systemd/system/phoxal-participant-navtask.service")
            .expect("navtask unit rendered");
        assert!(participant_unit.contains("Type=notify"));

        let payload_robot =
            std::fs::read_to_string(payload_opt(payload.root.path()).join("robot.yaml"))?;
        assert!(
            payload_robot.starts_with("schema: robot/v0"),
            "payload robot.yaml must keep the schema tag:\n{payload_robot}"
        );
        phoxal::model::robot::Robot::parse_from_string(&payload_robot)
            .expect("payload robot.yaml must round-trip through the version dispatcher");
        assert!(participant_unit.contains("WatchdogSec=10s"));
        assert!(participant_unit.contains("ExecStart=/opt/phoxal/active/bin/navtask"));
        assert!(
            payload
                .env_files
                .contains_key("/opt/phoxal/active/env/navtask.env")
        );
        assert_eq!(payload.release_json["schema"], RELEASE_SCHEMA);
        let release_artifact_ids = payload.release_json["artifacts"]
            .as_array()
            .expect("release artifacts should be an array")
            .iter()
            .filter_map(|artifact| artifact["id"].as_str())
            .collect::<BTreeSet<_>>();
        assert!(
            release_artifact_ids.contains("phoxal/infrastructure-router"),
            "official infrastructure release record should use package identity: {:?}",
            payload.release_json["artifacts"]
        );
        assert!(payload.install_plan.scoped_delete.is_empty());
        assert_eq!(
            payload.download_descriptor.schema,
            DOWNLOAD_DESCRIPTOR_SCHEMA
        );
        let official = payload.release_json["artifacts"]
            .as_array()
            .unwrap()
            .iter()
            .find(|artifact| artifact["id"] == "phoxal/infrastructure-router")
            .unwrap();
        assert_eq!(official["target"], "aarch64-unknown-linux-gnu");
        assert!(official["url"].as_str().unwrap().starts_with("https://"));
        assert!(
            payload
                .install_plan
                .units
                .contains(&"phoxal-participant-navtask.service".to_string())
        );
        Ok(())
    }

    #[test]
    fn payload_stages_path_component_metadata_and_structures() -> Result<()> {
        let _phoxal_home = ScratchPhoxalHome::new()?;
        let temp = tempfile::tempdir()?;
        phoxal_cli_test_support::write_bench_camera_project(temp.path())?;
        let payload = prepare_deploy(
            temp.path(),
            &dry_options(),
            target_for_arch("aarch64"),
            false,
            DRY_RUN_REMOTE_USER,
            &crate::Ui::from_env(),
        )?;
        let opt = payload_opt(payload.root.path());
        let metadata_files = payload_relative_files(payload.root.path())?
            .into_iter()
            .filter(|path| path == "structure.urdf" || path.starts_with("components/"))
            .collect::<Vec<_>>();

        assert_eq!(
            metadata_files,
            vec![
                "components/bench_camera/component.yaml".to_string(),
                "components/bench_camera/structure.urdf".to_string(),
                "structure.urdf".to_string(),
            ]
        );
        assert!(!opt.join("components/bench_camera/Cargo.toml").exists());
        assert!(!opt.join("components/bench_camera/src").exists());
        assert!(!opt.join("components/bench_camera/target").exists());
        assert!(
            payload
                .install_plan
                .direct_writes
                .contains(&"/opt/phoxal/active/structure.urdf".to_string())
        );
        assert!(
            payload
                .install_plan
                .direct_writes
                .contains(&"/opt/phoxal/active/components/bench_camera/component.yaml".to_string())
        );
        assert!(
            payload
                .install_plan
                .direct_writes
                .contains(&"/opt/phoxal/active/components/bench_camera/structure.urdf".to_string())
        );
        Ok(())
    }

    #[test]
    fn payload_without_path_components_has_no_components_dir() -> Result<()> {
        let _phoxal_home = ScratchPhoxalHome::new()?;
        let temp = tempfile::tempdir()?;
        phoxal_cli_test_support::write_catalog_only_project(temp.path())?;
        // This fixture's component pin is a git (not path) pin with a bogus
        // repository. Deploy metadata staging must skip it without trying
        // `git ls-remote` or `git clone`, so unlike the other fixtures it
        // carries no `robot.dev.yaml` overlay to load.
        let payload = prepare_deploy(
            temp.path(),
            &DeployOptions {
                overlays: Vec::new(),
                ..dry_options()
            },
            target_for_arch("aarch64"),
            false,
            DRY_RUN_REMOTE_USER,
            &crate::Ui::from_env(),
        )?;

        assert!(!payload_opt(payload.root.path()).join("components").exists());
        assert!(
            payload_opt(payload.root.path())
                .join("structure.urdf")
                .is_file()
        );
        Ok(())
    }

    #[test]
    fn sync_payload_stages_opt_tree_and_invokes_install_payload_helper() -> Result<()> {
        let _phoxal_home = ScratchPhoxalHome::new()?;
        let temp = tempfile::tempdir()?;
        write_basic_project(temp.path())?;
        let payload = prepare_deploy(
            temp.path(),
            &dry_options(),
            target_for_arch("aarch64"),
            false,
            DRY_RUN_REMOTE_USER,
            &crate::Ui::from_env(),
        )?;
        let remote_tmp = remote_staging_dir(PAYLOAD_STAGING_PREFIX);
        let mut remote = FakePayloadRemote::new("robot@test");

        sync_payload_via_helper(&mut remote, &payload, &remote_tmp)?;

        assert!(remote_tmp.starts_with(PAYLOAD_STAGING_PREFIX));
        assert_eq!(
            remote.ssh_statuses,
            vec![
                vec!["rm".to_string(), "-rf".to_string(), remote_tmp.clone()],
                vec!["mkdir".to_string(), "-p".to_string(), remote_tmp.clone()],
                vec!["rm".to_string(), "-rf".to_string(), remote_tmp.clone()],
            ]
        );
        assert_eq!(remote.rsyncs.len(), 1);
        let rsync = &remote.rsyncs[0];
        assert_eq!(rsync[0], "-az");
        assert_eq!(rsync[1], "--delete");
        assert_eq!(
            rsync[2],
            payload_opt(payload.root.path())
                .join("")
                .to_string_lossy()
                .into_owned()
        );
        assert_eq!(rsync[3], format!("robot@test:{remote_tmp}/"));
        assert_eq!(
            remote.helper_calls,
            vec![vec![
                "prepare-release".to_string(),
                remote_tmp,
                payload.install_plan.release_generation.clone(),
            ]]
        );
        Ok(())
    }

    #[test]
    fn helper_script_prepare_release_rejects_unsafe_staging_sources() {
        let script = helper_script();

        assert!(script.contains("prepare-release)"), "{script}");
        assert!(
            script.contains("valid_payload_source \"$source\" || exit 64"),
            "{script}"
        );
        assert!(script.contains("/tmp/phoxal-payload-*"), "{script}");
        assert!(
            script.contains("\"\"|*[!A-Za-z0-9_.@-]*) return 1 ;;"),
            "{script}"
        );
        assert!(
            script.contains("suffix=\"${1#/tmp/phoxal-payload-}\""),
            "{script}"
        );
    }

    #[test]
    fn helper_script_prepares_downloads_and_atomically_activates() {
        let script = helper_script();

        assert!(
            script.contains("cp -a \"$source/.\" \"$partial/\""),
            "{script}"
        );
        assert!(script.contains("$expected_sha.partial"), "{script}");
        assert!(script.contains("--retry 2 --retry-all-errors"), "{script}");
        assert!(
            script.contains("[ \"$actual_size\" = \"$expected_size\" ]"),
            "{script}"
        );
        assert!(
            script.contains("[ \"$actual_sha\" = \"$expected_sha\" ]"),
            "{script}"
        );
        assert!(script.contains("mv \"$partial\" \"$archive\""), "{script}");
        assert!(
            script.contains("mv -Tf \"$opt_root/.active.partial\" \"$opt_root/active\""),
            "{script}"
        );
        assert!(script.contains("rollback-release)"), "{script}");
    }

    #[test]
    fn helper_script_restart_target_resets_failed_units_before_restart() {
        let script = helper_script();
        let reset = script
            .find("systemctl reset-failed 'phoxal*' || true")
            .expect("restart-target should reset failed phoxal units");
        let restart = script
            .find("systemctl restart phoxal.target")
            .expect("restart-target should restart phoxal.target");

        assert!(reset < restart, "{script}");
    }

    #[test]
    fn helper_and_stale_cleanup_accept_generated_site_tool_units() {
        let script = helper_script();
        assert!(script.contains("phoxal-tool-*.service"), "{script}");
        assert!(managed_unit_name("phoxal-tool-joypad.service"));
        assert!(managed_unit_name("phoxal-tool-telemetry.service"));
        assert!(!managed_unit_name("phoxal-tool-../escape.service"));

        assert_eq!(
            stale_units(
                &["phoxal-tool-telemetry.service".to_string()],
                &["phoxal.target".to_string()],
            ),
            vec!["phoxal-tool-telemetry.service".to_string()]
        );
    }

    #[test]
    fn helper_script_is_valid_posix_shell() -> Result<()> {
        let mut child = Command::new("sh").arg("-n").stdin(Stdio::piped()).spawn()?;
        child
            .stdin
            .take()
            .context("shell syntax-check stdin missing")?
            .write_all(helper_script().as_bytes())?;
        assert!(child.wait()?.success());
        Ok(())
    }

    #[test]
    fn helper_script_hash_is_stable() {
        assert_eq!(
            helper_script_sha256(),
            "e29c0dc203a8e3bae7d8b93fdf36518a39fe5f85f293f31c15501f15c169c01e"
        );
    }

    fn test_bootstrap_scripts(remote_user: &str) -> BootstrapScripts {
        BootstrapScripts {
            helper_script: helper_script(),
            sudoers_fragment: sudoers_fragment(),
            remote_user: remote_user.to_string(),
        }
    }

    #[test]
    fn bootstrap_script_creates_phoxal_deploy_group_and_enrolls_remote_user() {
        let script = bootstrap_script(&test_bootstrap_scripts("jetson-op"));
        assert!(
            script.contains("if ! getent group phoxal-deploy >/dev/null; then"),
            "{script}"
        );
        assert!(
            script.contains("groupadd --system phoxal-deploy"),
            "{script}"
        );
        assert!(
            script.contains("usermod -aG phoxal-deploy -- jetson-op"),
            "{script}"
        );
    }

    #[test]
    fn bootstrap_script_terminates_usermod_options_before_remote_user() {
        let script = bootstrap_script(&test_bootstrap_scripts("-operator"));
        assert!(
            script.contains("usermod -aG phoxal-deploy -- -operator"),
            "{script}"
        );
    }

    #[test]
    fn deploy_ssh_commands_disable_connection_multiplexing() {
        let command = deploy_ssh_command();
        assert_eq!(
            command
                .get_args()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect::<Vec<_>>(),
            [
                "-o",
                "ControlMaster=no",
                "-o",
                "ControlPath=none",
                "-o",
                "ControlPersist=no",
            ]
        );
    }

    #[test]
    fn bootstrap_script_writes_the_static_group_fragment() {
        let script = bootstrap_script(&test_bootstrap_scripts("jetson-op"));
        assert!(
            script.contains(
                "%phoxal-deploy ALL=(root) NOPASSWD: /usr/local/sbin/phoxal-systemd-helper *"
            ),
            "{script}"
        );
        // The static fragment must not mention the enrolled user by name.
        assert!(!script.contains("jetson-op ALL=(root)"), "{script}");
    }

    #[test]
    fn bootstrap_script_is_valid_posix_shell() -> Result<()> {
        let script = bootstrap_script(&test_bootstrap_scripts("jetson-op"));
        let mut child = Command::new("sh").arg("-n").stdin(Stdio::piped()).spawn()?;
        child
            .stdin
            .take()
            .context("shell syntax-check stdin missing")?
            .write_all(script.as_bytes())?;
        assert!(child.wait()?.success(), "{script}");
        Ok(())
    }

    #[test]
    fn validate_remote_username_accepts_conservative_charset() {
        for user in ["robot", "jetson-op", "user.name", "user_name", "a@b-c.d"] {
            assert!(
                validate_remote_username(user).is_ok(),
                "{user} should be accepted"
            );
        }
    }

    #[test]
    fn validate_remote_username_rejects_hostile_input() {
        for user in [
            "",
            "evil'; rm -rf /",
            "user name",
            "user$(whoami)",
            "user\nrm -rf /",
            "user;ls",
            "user`ls`",
        ] {
            let error = validate_remote_username(user)
                .err()
                .unwrap_or_else(|| panic!("{user:?} should be rejected"));
            assert!(
                error.to_string().contains("DeployInvalidRemoteUser"),
                "{error}"
            );
        }
    }

    #[test]
    fn render_payload_rejects_a_hostile_remote_user() {
        let _phoxal_home = ScratchPhoxalHome::new().expect("scratch phoxal home");
        let temp = tempfile::tempdir().expect("tempdir");
        write_basic_project(temp.path()).expect("write project");
        let mut transport = FakeTransport::healthy();
        transport.probe.remote_user = "evil'; rm -rf /".to_string();
        let error = deploy_with_transport(
            temp.path(),
            &live_options(),
            &mut transport,
            false,
            &crate::Ui::from_env(),
        )
        .expect_err("a hostile remote user must be rejected before bootstrapping anything");
        assert!(
            error.to_string().contains("DeployInvalidRemoteUser"),
            "{error}"
        );
        assert!(
            !transport.bootstrapped,
            "bootstrap must never run with an unvalidated remote user"
        );
    }

    #[test]
    fn download_executor_is_bounded_and_processes_every_artifact() -> Result<()> {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let active = AtomicUsize::new(0);
        let maximum = AtomicUsize::new(0);
        let completed = AtomicUsize::new(0);
        let artifacts = (0..11).collect::<Vec<_>>();
        run_bounded(&artifacts, DOWNLOAD_CONCURRENCY, |_| {
            let now = active.fetch_add(1, Ordering::SeqCst) + 1;
            maximum.fetch_max(now, Ordering::SeqCst);
            std::thread::yield_now();
            completed.fetch_add(1, Ordering::SeqCst);
            active.fetch_sub(1, Ordering::SeqCst);
            Ok(())
        })?;

        assert_eq!(completed.load(Ordering::SeqCst), artifacts.len());
        assert!(maximum.load(Ordering::SeqCst) <= DOWNLOAD_CONCURRENCY);
        Ok(())
    }

    #[test]
    fn driver_graph_renders_one_unit_per_instance_with_privileges() -> Result<()> {
        let _phoxal_home = ScratchPhoxalHome::new()?;
        let temp = tempfile::tempdir()?;
        phoxal_cli_test_support::write_driver_project(temp.path())?;
        let payload = prepare_deploy(
            temp.path(),
            &dry_options(),
            target_for_arch("aarch64"),
            false,
            DRY_RUN_REMOTE_USER,
            &crate::Ui::from_env(),
        )?;
        let left = payload
            .rendered_units
            .get("/etc/systemd/system/phoxal-participant-left_drive.service")
            .expect("left unit");
        let right = payload
            .rendered_units
            .get("/etc/systemd/system/phoxal-participant-right_drive.service")
            .expect("right unit");
        assert!(left.contains("DeviceAllow=/dev/ttyUSB0 rw"));
        assert!(left.contains("SupplementaryGroups=dialout"));
        assert!(right.contains("DeviceAllow=/dev/i2c-1 rw"));
        assert!(right.contains("SupplementaryGroups=i2c"));
        assert!(left.contains("ExecStart=/opt/phoxal/active/bin/driver-ddsm115"));
        assert!(right.contains("ExecStart=/opt/phoxal/active/bin/driver-ddsm115"));
        Ok(())
    }

    fn resolved_with_components(
        components: Vec<crate::resolver::ResolvedComponent>,
    ) -> Result<ResolvedRobot> {
        Ok(ResolvedRobot {
            robot: Robot::parse_from_string(MINIMAL_RESOLVED_ROBOT_YAML)?,
            channel: crate::catalog::SelectionChannel::Stable,
            target: crate::resolver::host_target_triple(),
            catalog_snapshot: None,
            platform_runtimes: Vec::new(),
            simulators: Vec::new(),
            user_runtimes: Vec::new(),
            components,
            tools: Vec::new(),
            path_overrides: Vec::new(),
        })
    }

    const MINIMAL_RESOLVED_ROBOT_YAML: &str = r#"schema: robot/v0
robot:
  id: testbot
  namespace: test
  motion_limits:
    max_linear_speed_mps: 0.6
    max_angular_speed_radps: 2.0
  kinematic:
    kind: omnidirectional
    actuators: []
    encoders: []
  components: {}
"#;

    /// A Catalog-sourced component package with a populated `catalog_runtime`
    /// but nothing warmed in the local artifact cache - the shape a fresh
    /// clean machine sees before any `phoxal update`.
    fn cold_cache_catalog_component_package(
        package: &str,
        kind: crate::catalog::ArtifactKind,
        component_name: &str,
    ) -> crate::resolver::ResolvedComponentPackage {
        crate::resolver::ResolvedComponentPackage {
            package: package.to_string(),
            kind,
            source: ResolvedComponentSource::Catalog,
            path_override: None,
            catalog_runtime: Some(ResolvedPlatformRuntime {
                name: component_name.to_string(),
                package: package.to_string(),
                kind,
                version: "0.1.0".to_string(),
                artifact_ref: format!(
                    "phoxal-component-{component_name}-{}-v0.1.0-aarch64-unknown-linux-gnu.tar.zst",
                    kind.catalog_kind()
                ),
                sha256: Some("a".repeat(64)),
                url: Some("https://example.invalid/component.tar.zst".to_string()),
                size: Some(1),
                published: true,
                published_triples: Vec::new(),
                path_override: None,
                channel: crate::catalog::SelectionChannel::Stable,
                target: Some("aarch64-unknown-linux-gnu".to_string()),
            }),
        }
    }

    /// Selects a scratch project-local store so this test's "nothing vendored
    /// yet" assumption is exact. Shared crate-wide because the project-root
    /// test override is process-global and unit tests run concurrently.
    use crate::host_paths::test_support::ScratchPhoxalHome;

    #[test]
    fn dry_run_stays_offline_for_catalog_resolved_component_driver() -> Result<()> {
        // Band B kept `deploy --dry-run` from resolving git component
        // commits so it never touches the network; a Catalog-sourced
        // component driver/assets pair must uphold the identical guarantee.
        // This exercises exactly the two functions `render_payload` calls to
        // stage a component's driver binary / assets bundle
        // (`stage_official_artifacts`'s runtime lookup and
        // `locate_cached_component_assets_dir`) directly against a cold
        // cache, bypassing the graph-check emit-apis fetch (a pre-existing,
        // symmetric-with-services network dependency that is unrelated to
        // this staging step and out of this change's scope). Observing a
        // clean, local-only result - `NativePending`-eligible missing
        // binary, no cached assets dir - proves neither function reaches
        // for the network; this process has real internet access, so a
        // download attempt against the fixture's made-up (unpublished)
        // asset name would surface as a loud HTTP/connection error, not a
        // silent hang.
        let _phoxal_home = ScratchPhoxalHome::new()?;

        let driver_package = cold_cache_catalog_component_package(
            "phoxal/component-ddsm115",
            crate::catalog::ArtifactKind::ComponentDriver,
            "ddsm115",
        );
        let assets_package = cold_cache_catalog_component_package(
            "phoxal/component-ddsm115",
            crate::catalog::ArtifactKind::ComponentAssets,
            "ddsm115",
        );

        let mut resolved = resolved_with_components(vec![ResolvedComponent {
            instance: "left_drive".to_string(),
            source_name: "ddsm115".to_string(),
            assets: Some(assets_package.clone()),
            driver: Some(driver_package),
            has_driver: true,
        }])?;
        resolved.tools.push(ResolvedTool {
            kind: crate::catalog::ArtifactKind::Infrastructure,
            name: SITE_INFRASTRUCTURE_ROUTER.to_string(),
            package: "phoxal/infrastructure-router".to_string(),
            requested: "0.1.0".to_string(),
            resolved: "0.1.0".to_string(),
            repo: "phoxal/framework".to_string(),
            asset: "phoxal-infrastructure-router-0.1.0-aarch64-unknown-linux-gnu.tar.zst"
                .to_string(),
            binary_name: "phoxal-infrastructure-router".to_string(),
            sha256: "0".repeat(64),
            url: None,
            size: None,
            published: true,
            path_override: Some(PathBuf::from("/fake/router")),
            channel: crate::catalog::SelectionChannel::Stable,
            target: "aarch64-unknown-linux-gnu".to_string(),
        });

        // 1) `official_runtime_by_artifact_id` finds the driver's
        //    `catalog_runtime` (proving it is visible through the same
        //    lookup a service uses).
        let found = official_runtime_by_artifact_id(&resolved, "ddsm115")
            .expect("catalog driver runtime must be discoverable by its artifact id");
        assert_eq!(found.package, "phoxal/component-ddsm115");

        // 2) `official_runtime_plan` (what `stage_official_artifacts` calls)
        //    reports the binary as locally absent rather than downloading -
        //    a cold cache yields `source_path: None`, which the caller
        //    turns into `NativePending` for a live deploy or a tolerated
        //    "missing" entry for `--dry-run`.
        let root = tempfile::tempdir()?;
        let plan = official_runtime_plan(root.path(), found, false)?;
        assert!(
            plan.source_path.is_none(),
            "a cold cache must report no local binary, not download one"
        );
        assert!(plan.missing_label.is_none(), "artifact is published");

        // 3) `locate_cached_component_assets_dir` returns `None` on a cold
        //    cache instead of fetching the assets bundle.
        assert_eq!(
            locate_cached_component_assets_dir(&assets_package)?,
            None,
            "a cold cache must report no cached assets dir, not download one"
        );

        Ok(())
    }

    /// CLI-UX Phase 4: deploy now ships EVERY standard site tool
    /// (`tool-router`, `tool-joypad`, `tool-telemetry`), not just the
    /// router - each gets its own unit ordered after the router, and
    /// `tool-joypad` carries the `/dev/input` tool-privilege grant
    /// (`unit_privileges_for_tool`). `write_basic_project`'s fixture catalog
    /// auto-fills every `OFFICIAL_TOOLS`/`OFFICIAL_OPTIONAL_TOOLS` entry
    /// (`catalog::fixture_catalog_for_tests`), so all three resolve here.
    #[test]
    fn privileged_tool_graph_renders_router_joypad_and_telemetry_units() -> Result<()> {
        let _phoxal_home = ScratchPhoxalHome::new()?;
        let temp = tempfile::tempdir()?;
        write_basic_project(temp.path())?;
        let payload = prepare_deploy(
            temp.path(),
            &dry_options(),
            target_for_arch("aarch64"),
            false,
            DRY_RUN_REMOTE_USER,
            &crate::Ui::from_env(),
        )?;
        assert!(
            payload
                .rendered_units
                .contains_key("/etc/systemd/system/phoxal-router.service")
        );
        let joypad_unit = payload
            .rendered_units
            .get("/etc/systemd/system/phoxal-tool-joypad.service")
            .expect("tool-joypad unit must render");
        assert!(joypad_unit.contains("After=network-online.target phoxal-router.service"));
        assert!(joypad_unit.contains("SupplementaryGroups=input"));
        assert!(joypad_unit.contains("DeviceAllow=/dev/input/* rw"));
        let telemetry_unit = payload
            .rendered_units
            .get("/etc/systemd/system/phoxal-tool-telemetry.service")
            .expect("tool-telemetry unit must render");
        assert!(telemetry_unit.contains("After=network-online.target phoxal-router.service"));
        assert!(!telemetry_unit.contains("SupplementaryGroups="));
        assert!(!telemetry_unit.contains("DeviceAllow="));
        assert!(
            !payload
                .env_files
                .contains_key("/opt/phoxal/active/env/router.env")
        );
        for env_name in ["tool-joypad.env", "tool-telemetry.env"] {
            let contents = payload
                .env_files
                .get(&format!("/opt/phoxal/active/env/{env_name}"))
                .unwrap_or_else(|| panic!("{env_name} must render"));
            assert!(
                !contents.contains("PHOXAL_CLOCK="),
                "tools must not receive a clock selection in {env_name}:\n{contents}"
            );
        }
        // Stale-unit cleanup and unit installation both key off `unit_names`
        // generically - proving the new units are IN that list (not just
        // rendered as file content) is what actually wires them into
        // install/health-check/restart.
        assert!(
            payload
                .unit_names
                .contains(&"phoxal-tool-joypad.service".to_string())
        );
        assert!(
            payload
                .unit_names
                .contains(&"phoxal-tool-telemetry.service".to_string())
        );
        Ok(())
    }

    #[test]
    fn rejected_non_immutable_artifact_gets_designed_error() -> Result<()> {
        let _phoxal_home = ScratchPhoxalHome::new()?;
        let temp = tempfile::tempdir()?;
        phoxal_cli_test_support::write_native_dep_project(temp.path())?;
        let error = prepare_deploy(
            temp.path(),
            &dry_options(),
            target_for_arch("aarch64"),
            false,
            DRY_RUN_REMOTE_USER,
            &crate::Ui::from_env(),
        )
        .expect_err("native C deps should be rejected before raw linker spew");
        let message = error.to_string();
        assert!(message.contains("CrossBuildUnsupported"), "{message}");
        assert!(message.contains("opencv"), "{message}");
        Ok(())
    }

    #[test]
    #[cfg(unix)]
    fn missing_zig_toolchain_path_gets_designed_fix() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let bin = temp.path().join("bin");
        fs::create_dir_all(&bin)?;
        write_test_executable(&bin.join("cargo"), "#!/bin/sh\nexit 0\n")?;
        let base = OsString::from("/definitely-not-on-path");
        let search_path = path_with_cache_bin(&bin, Some(base.as_os_str()))?;

        let error = validate_zigbuild_toolchain(&search_path, &bin)
            .expect_err("missing zig should be diagnosed before build");
        let message = error.to_string();

        assert!(message.contains("CrossBuildToolchainMissing"), "{message}");
        assert!(message.contains("zig is required"), "{message}");
        assert!(message.contains("brew install zig"), "{message}");
        Ok(())
    }

    #[test]
    #[cfg(unix)]
    fn missing_cargo_zigbuild_path_gets_designed_fix() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let bin = temp.path().join("bin");
        fs::create_dir_all(&bin)?;
        write_test_executable(&bin.join("cargo"), "#!/bin/sh\nexit 1\n")?;
        write_test_executable(&bin.join("zig"), "#!/bin/sh\nexit 0\n")?;
        let base = OsString::from("/definitely-not-on-path");
        let search_path = path_with_cache_bin(&bin, Some(base.as_os_str()))?;

        let error = validate_zigbuild_toolchain(&search_path, &bin)
            .expect_err("missing cargo-zigbuild should be diagnosed before build");
        let message = error.to_string();

        assert!(message.contains("CrossBuildToolchainMissing"), "{message}");
        assert!(message.contains("cargo-zigbuild 0.23.0"), "{message}");
        assert!(
            message.contains("cargo install cargo-zigbuild --locked --version 0.23.0"),
            "{message}"
        );
        Ok(())
    }

    #[test]
    fn zigbuild_failure_classifies_native_sysroot_crate() {
        let message = classify_zigbuild_failure(
            "vision",
            "aarch64-unknown-linux-musl",
            b"",
            b"error: failed to run custom build command for `opencv v0.92.0`\n\
              pkg-config has not been configured to support cross-compilation\n",
        );

        assert!(message.contains("CrossBuildUnsupported"), "{message}");
        assert!(message.contains("opencv"), "{message}");
        assert!(
            message.contains("target-native system headers/libs"),
            "{message}"
        );
    }

    #[test]
    fn stale_unit_removal_is_computed_by_tree_comparison() -> Result<()> {
        let _phoxal_home = ScratchPhoxalHome::new()?;
        let temp = tempfile::tempdir()?;
        write_basic_project(temp.path())?;
        let mut transport = FakeTransport::healthy();
        transport.installed_units = vec![
            "phoxal.target".to_string(),
            "phoxal-router.service".to_string(),
            "phoxal-participant-old.service".to_string(),
        ];
        let report = deploy_with_transport(
            temp.path(),
            &live_options(),
            &mut transport,
            false,
            &crate::Ui::from_env(),
        )?;
        assert_eq!(
            transport.stale_removed,
            vec!["phoxal-participant-old.service"]
        );
        assert!(
            report
                .install_plan
                .stale_units_to_remove
                .contains(&"phoxal-participant-old.service".to_string())
        );
        assert!(transport.bootstrapped);
        assert!(transport.synced);
        assert!(transport.restarted);
        assert!(!transport.downloaded.is_empty());
        assert!(!transport.fallback_prepared);
        assert!(transport.activated.is_some());
        assert!(transport.finalized);
        assert_eq!(
            transport.previous_generation.as_deref(),
            Some("releases/previous-generation")
        );
        assert_eq!(report.delivery, Some(OfficialDelivery::RobotDownload));
        Ok(())
    }

    #[test]
    fn unreachable_github_uses_host_transfer_fallback() -> Result<()> {
        let _phoxal_home = ScratchPhoxalHome::new()?;
        let temp = tempfile::tempdir()?;
        write_basic_project(temp.path())?;
        let mut transport = FakeTransport::healthy();
        transport.github_reachable = false;

        let report = deploy_with_transport(
            temp.path(),
            &live_options(),
            &mut transport,
            false,
            &crate::Ui::from_env(),
        )?;

        assert!(transport.fallback_prepared);
        assert!(transport.downloaded.is_empty());
        assert!(transport.activated.is_some());
        assert_eq!(
            report.delivery,
            Some(OfficialDelivery::HostTransferFallback)
        );
        Ok(())
    }

    #[test]
    fn failed_artifact_verification_never_activates() -> Result<()> {
        let _phoxal_home = ScratchPhoxalHome::new()?;
        let temp = tempfile::tempdir()?;
        write_basic_project(temp.path())?;
        let mut transport = FakeTransport::healthy();
        transport.download_fails = true;

        let error = deploy_with_transport(
            temp.path(),
            &live_options(),
            &mut transport,
            false,
            &crate::Ui::from_env(),
        )
        .expect_err("verification failure must abort before activation");

        assert!(error.to_string().contains("robot failed to download"));
        assert!(transport.activated.is_none());
        assert!(!transport.restarted);
        assert!(!transport.rolled_back);
        Ok(())
    }

    #[cfg(unix)]
    fn write_test_executable(path: &Path, contents: &str) -> Result<()> {
        fs::write(path, contents)?;
        make_executable(path)?;
        Ok(())
    }

    fn probe(
        bootstrap_required: bool,
        sudo_noninteractive: bool,
        helper_grant: bool,
    ) -> RemoteProbe {
        probe_with_helper_stale(bootstrap_required, sudo_noninteractive, helper_grant, false)
    }

    fn probe_with_helper_stale(
        bootstrap_required: bool,
        sudo_noninteractive: bool,
        helper_grant: bool,
        helper_stale: bool,
    ) -> RemoteProbe {
        RemoteProbe {
            arch: "aarch64".to_string(),
            bootstrap_required,
            remote_user: "robot".to_string(),
            sudo_noninteractive,
            helper_grant,
            helper_stale,
        }
    }

    #[test]
    fn sudo_probe_row1_noninteractive_sudo_always_proceeds() {
        // sudo -n true works: proceed regardless of bootstrap/grant state or
        // local tty - any root work runs non-interactively and no password
        // source is touched.
        for probe in [
            probe(true, true, true),
            probe(false, true, true),
            probe(false, true, false),
        ] {
            let mut transport = FakeTransport::healthy();
            let mut source = ScriptedSudoPasswordSource::none();
            let password = ensure_sudo_will_succeed(
                "robot@jetson",
                &probe,
                false,
                &mut source,
                &mut transport,
            )
            .expect("row 1 should proceed");
            assert!(password.is_none());
            assert_eq!(source.env_calls, 0);
            assert_eq!(source.prompt_calls, 0);
            assert!(transport.validation_password_stdin.is_empty());
        }
    }

    #[test]
    fn sudo_probe_row2_helper_grant_for_this_user_proceeds_without_tty() {
        // No blanket sudo, but the installed helper's per-command grant
        // covers this user and the helper hash matches: steady-state deploy,
        // no root work needed and no password source is touched.
        let probe = probe(false, false, true);
        let mut transport = FakeTransport::healthy();
        let mut source = ScriptedSudoPasswordSource::none();
        let password =
            ensure_sudo_will_succeed("robot@jetson", &probe, false, &mut source, &mut transport)
                .expect("row 2 should proceed");
        assert!(password.is_none());
        assert_eq!(source.env_calls, 0);
        assert_eq!(source.prompt_calls, 0);
        assert!(transport.validation_password_stdin.is_empty());
    }

    #[test]
    fn sudo_probe_row3_root_work_with_tty_prompts_and_validates() {
        // Root work required (first bootstrap, or stale grant repair) and
        // local /dev/tty is available: read one password and validate it now.
        let probe = probe(true, false, false);
        let mut transport = FakeTransport::healthy();
        let mut source = ScriptedSudoPasswordSource::with_prompts(&["secret"]);
        let password =
            ensure_sudo_will_succeed("robot@jetson", &probe, true, &mut source, &mut transport)
                .expect("row 3 should proceed after a valid password");

        assert!(password.is_some());
        assert_eq!(source.env_calls, 1);
        assert_eq!(source.prompt_calls, 1);
        assert_eq!(
            source.prompts_seen,
            vec!["[sudo] password for robot on robot@jetson:".to_string()]
        );
        assert_eq!(
            transport.validation_password_stdin,
            vec![b"secret\n".to_vec()]
        );
    }

    #[test]
    fn sudo_probe_validation_failure_retries_once_then_errors() {
        let probe = probe(true, false, false);
        let mut transport = FakeTransport::healthy();
        transport.validation_results = VecDeque::from([false, false]);
        let mut source = ScriptedSudoPasswordSource::with_prompts(&["bad", "still-bad"]);

        let error =
            ensure_sudo_will_succeed("robot@jetson", &probe, true, &mut source, &mut transport)
                .err()
                .expect("two failed sudo validations should stop deploy");
        let message = error.to_string();

        assert!(message.contains("DeploySudoPasswordRejected"), "{message}");
        assert!(message.contains("robot@jetson"), "{message}");
        assert_eq!(source.prompt_calls, 2);
        assert_eq!(
            transport.validation_password_stdin,
            vec![b"bad\n".to_vec(), b"still-bad\n".to_vec()]
        );
        assert!(!transport.bootstrapped);
    }

    #[test]
    fn sudo_probe_env_password_without_tty_proceeds() {
        let probe = probe(true, false, false);
        let mut transport = FakeTransport::healthy();
        let mut source = ScriptedSudoPasswordSource::with_env("env-secret");
        let password =
            ensure_sudo_will_succeed("robot@jetson", &probe, false, &mut source, &mut transport)
                .expect("env password should satisfy root work without a tty");

        assert!(password.is_some());
        assert_eq!(source.env_calls, 1);
        assert_eq!(source.prompt_calls, 0);
        assert_eq!(
            transport.validation_password_stdin,
            vec![b"env-secret\n".to_vec()]
        );
    }

    #[test]
    fn row3_deploy_bootstrap_uses_sudo_s_and_writes_password_once() -> Result<()> {
        let _phoxal_home = ScratchPhoxalHome::new()?;
        let temp = tempfile::tempdir()?;
        write_basic_project(temp.path())?;
        let mut transport = FakeTransport::healthy();
        transport.probe.sudo_noninteractive = false;
        transport.probe.helper_grant = false;
        let mut source = ScriptedSudoPasswordSource::with_prompts(&["secret"]);

        deploy_with_transport_with_sudo(
            temp.path(),
            &live_options(),
            &mut transport,
            true,
            &mut source,
            &crate::Ui::from_env(),
        )?;

        assert!(transport.bootstrapped);
        assert_eq!(source.prompt_calls, 1);
        assert_eq!(
            transport.validation_password_stdin,
            vec![b"secret\n".to_vec()]
        );
        assert_eq!(
            transport.bootstrap_sudo_command_seen,
            Some(vec![
                "sudo".to_string(),
                "-S".to_string(),
                "-p".to_string(),
                SUDO_STDIN_PROMPT.to_string(),
                "sh".to_string(),
                "/tmp/phoxal-bootstrap.TEST.sh".to_string(),
            ])
        );
        assert_eq!(
            transport.bootstrap_password_stdin,
            vec![b"secret\n".to_vec()]
        );
        Ok(())
    }

    #[test]
    fn sudo_probe_row4_root_work_without_tty_fails_fast() {
        // Root work required and no local tty: fail before building
        // anything, naming the host and all remedies.
        let probe = probe(true, false, false);
        let mut transport = FakeTransport::healthy();
        let mut source = ScriptedSudoPasswordSource::none();
        let error =
            ensure_sudo_will_succeed("robot@jetson", &probe, false, &mut source, &mut transport)
                .err()
                .expect("non-interactive sudo with required bootstrap must fail fast");
        let message = error.to_string();
        assert!(message.contains("DeploySudoRequiresPassword"), "{message}");
        assert!(message.contains("robot@jetson"), "{message}");
        assert!(message.contains("robot"), "{message}");
        assert!(message.contains("first deploy"), "{message}");
        assert!(message.contains("interactively"), "{message}");
        assert!(message.contains("NOPASSWD"), "{message}");
        assert!(message.contains(SUDO_PASSWORD_ENV), "{message}");
    }

    #[test]
    fn sudo_probe_row4_stale_grant_without_tty_fails_fast_naming_repair() {
        // Bootstrapped host, but this user is not covered by the group grant
        // (`sudo -n true` fails and the helper grant probe fails):
        // blanket-sudo success must not be inferred from the helper being
        // installed - fail fast and name the group-grant repair rather than
        // the first install.
        let probe = probe(false, false, false);
        let mut transport = FakeTransport::healthy();
        let mut source = ScriptedSudoPasswordSource::none();
        let error =
            ensure_sudo_will_succeed("robot@jetson", &probe, false, &mut source, &mut transport)
                .err()
                .expect("stale helper grant without a tty must fail fast, not die mid-flight");
        let message = error.to_string();
        assert!(message.contains("DeploySudoRequiresPassword"), "{message}");
        assert!(message.contains("robot@jetson"), "{message}");
        assert!(
            message.contains("not covered by the phoxal-deploy group grant"),
            "{message}"
        );
        assert!(message.contains("add this user to the group"), "{message}");
        assert!(!message.contains("first deploy"), "{message}");
        assert!(message.contains("NOPASSWD"), "{message}");
    }

    #[test]
    fn sudo_probe_row4_stale_helper_without_tty_fails_fast_naming_repair() {
        let probe = probe_with_helper_stale(false, false, true, true);
        let mut transport = FakeTransport::healthy();
        let mut source = ScriptedSudoPasswordSource::none();
        let error =
            ensure_sudo_will_succeed("robot@jetson", &probe, false, &mut source, &mut transport)
                .err()
                .expect("stale helper without a tty must fail fast");
        let message = error.to_string();
        assert!(message.contains("DeploySudoRequiresPassword"), "{message}");
        assert!(message.contains("stale"), "{message}");
        assert!(message.contains("rewrite the helper"), "{message}");
        assert!(!message.contains("first deploy"), "{message}");
    }

    #[test]
    fn sudoers_fragment_is_the_static_group_grant() {
        let fragment = sudoers_fragment();
        assert_eq!(
            fragment,
            "%phoxal-deploy ALL=(root) NOPASSWD: /usr/local/sbin/phoxal-systemd-helper *\n"
        );
        // Calling it again (as a second operator's deploy would) must
        // produce the identical fragment - nobody's grant gets revoked by
        // someone else's deploy.
        assert_eq!(fragment, sudoers_fragment());
    }

    #[test]
    fn deploy_with_transport_writes_static_fragment_and_enrolls_probed_user() -> Result<()> {
        let _phoxal_home = ScratchPhoxalHome::new()?;
        let temp = tempfile::tempdir()?;
        write_basic_project(temp.path())?;
        let mut transport = FakeTransport::healthy();
        transport.probe.remote_user = "jetson-op".to_string();
        deploy_with_transport(
            temp.path(),
            &live_options(),
            &mut transport,
            false,
            &crate::Ui::from_env(),
        )?;
        assert!(transport.bootstrapped);
        let fragment = transport
            .bootstrap_fragment_seen
            .expect("bootstrap should have been called with a fragment");
        assert_eq!(
            fragment,
            "%phoxal-deploy ALL=(root) NOPASSWD: /usr/local/sbin/phoxal-systemd-helper *\n"
        );
        assert_eq!(
            transport.bootstrap_remote_user_seen,
            Some("jetson-op".to_string())
        );
        Ok(())
    }

    #[test]
    fn stale_helper_grant_triggers_bootstrap_repair_over_tty() -> Result<()> {
        // Bootstrapped host (bootstrap_required false), but the grant probe
        // failed - e.g. this user has never deployed to this host, or the
        // host is still on the old per-user model. With a local tty the
        // deploy must re-run the bootstrap script (it rewrites the helper
        // and the fragment idempotently) and enroll the new deploying user
        // into the phoxal-deploy group.
        let _phoxal_home = ScratchPhoxalHome::new()?;
        let temp = tempfile::tempdir()?;
        write_basic_project(temp.path())?;
        let mut transport = FakeTransport::healthy();
        transport.probe.bootstrap_required = false;
        transport.probe.sudo_noninteractive = false;
        transport.probe.helper_grant = false;
        transport.probe.remote_user = "user-b".to_string();
        let mut source = ScriptedSudoPasswordSource::with_prompts(&["secret"]);
        deploy_with_transport_with_sudo(
            temp.path(),
            &live_options(),
            &mut transport,
            true,
            &mut source,
            &crate::Ui::from_env(),
        )?;
        assert!(
            transport.bootstrapped,
            "a missing/stale grant must re-run bootstrap even though /opt/phoxal exists"
        );
        let fragment = transport
            .bootstrap_fragment_seen
            .expect("bootstrap should have been called with a fragment");
        assert_eq!(
            fragment,
            "%phoxal-deploy ALL=(root) NOPASSWD: /usr/local/sbin/phoxal-systemd-helper *\n"
        );
        assert_eq!(
            transport.bootstrap_remote_user_seen,
            Some("user-b".to_string())
        );
        Ok(())
    }

    #[test]
    fn blanket_sudo_without_group_membership_still_triggers_bootstrap_repair() -> Result<()> {
        // A device with blanket passwordless sudo but no phoxal-deploy
        // membership (e.g. deployed under the old per-user model) must
        // still take the bootstrap/repair path so it converges to the group
        // model. Because sudo_noninteractive is true, this proceeds and
        // runs the repair non-interactively, without touching the local tty
        // or a password source at all.
        let _phoxal_home = ScratchPhoxalHome::new()?;
        let temp = tempfile::tempdir()?;
        write_basic_project(temp.path())?;
        let mut transport = FakeTransport::healthy();
        transport.probe.bootstrap_required = false;
        transport.probe.sudo_noninteractive = true;
        transport.probe.helper_grant = false;
        transport.probe.remote_user = "legacy-user".to_string();
        let mut source = ScriptedSudoPasswordSource::none();
        deploy_with_transport_with_sudo(
            temp.path(),
            &live_options(),
            &mut transport,
            false,
            &mut source,
            &crate::Ui::from_env(),
        )?;
        assert!(
            transport.bootstrapped,
            "blanket sudo without phoxal-deploy membership must still repair, not skip bootstrap"
        );
        assert_eq!(source.env_calls, 0);
        assert_eq!(source.prompt_calls, 0);
        assert_eq!(
            transport.bootstrap_remote_user_seen,
            Some("legacy-user".to_string())
        );
        Ok(())
    }

    #[test]
    fn stale_helper_hash_triggers_bootstrap_repair_with_existing_grant() -> Result<()> {
        let _phoxal_home = ScratchPhoxalHome::new()?;
        let temp = tempfile::tempdir()?;
        write_basic_project(temp.path())?;
        let mut transport = FakeTransport::healthy();
        transport.probe.bootstrap_required = false;
        transport.probe.sudo_noninteractive = false;
        transport.probe.helper_grant = true;
        transport.probe.helper_stale = true;
        let mut source = ScriptedSudoPasswordSource::with_prompts(&["secret"]);
        deploy_with_transport_with_sudo(
            temp.path(),
            &live_options(),
            &mut transport,
            true,
            &mut source,
            &crate::Ui::from_env(),
        )?;
        assert!(
            transport.bootstrapped,
            "a stale helper must re-run bootstrap even when the helper grant is valid"
        );
        Ok(())
    }

    #[test]
    fn failed_health_push_exits_nonzero_with_diagnosis() -> Result<()> {
        let _phoxal_home = ScratchPhoxalHome::new()?;
        let temp = tempfile::tempdir()?;
        write_basic_project(temp.path())?;
        let mut transport = FakeTransport::healthy();
        transport.health = HealthReport {
            units: vec![HealthUnitReport {
                unit: "phoxal-participant-navtask.service".to_string(),
                participant: Some("navtask".to_string()),
                ready: false,
                active_state: "failed".to_string(),
                sub_state: "failed".to_string(),
                journal_excerpt: vec!["boom".to_string()],
            }],
        };
        let error = deploy_with_transport(
            temp.path(),
            &live_options(),
            &mut transport,
            false,
            &crate::Ui::from_env(),
        )
        .expect_err("health failure should fail deploy");
        let message = error.to_string();
        assert!(message.contains("HealthReportFailed"), "{message}");
        assert!(message.contains("navtask"), "{message}");
        assert!(message.contains("boom"), "{message}");
        assert!(message.contains("rolled back"), "{message}");
        assert!(transport.rolled_back);
        assert!(!transport.finalized);
        assert_eq!(
            transport.active_generation.as_deref(),
            Some("releases/previous-generation")
        );
        assert!(
            transport
                .previous_generation
                .as_deref()
                .is_some_and(|generation| generation != "releases/previous-generation")
        );
        Ok(())
    }
}
