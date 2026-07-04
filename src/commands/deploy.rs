use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use clap::Args;
use phoxal::model::robot::v1::{ConnectionConfig, Robot};
use phoxal::participant::launch::env;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use tempfile::TempDir;

use crate::AppContext;
use crate::catalog::{ArtifactKind, ArtifactStatus};
use crate::commands::MessageFormat;
use crate::commands::check::{
    CheckGraphContext, SourceParticipant, SourceParticipantKind, build_emit_apis_from_source,
    fetch_emit_apis_from_native_artifact, platform_artifact_refs_from_resolved,
    robot_graph_from_resolved, run_check_with_context, source_participants_from_resolved,
};
use crate::component_driver::component_crate_dir;
use crate::launch_env::{EncodedParticipantEnv, encode_participant_env};
use crate::launch_plan::{
    CheckedRobotLaunchInput, LaunchMode, LaunchPlan, ParticipantExecution, ParticipantLaunchRecord,
    SITE_TOOL_ROUTER, SiteLaunch, build_launch_plan,
};
use crate::resolver::{
    ResolveOptions, ResolvedComponentSource, ResolvedPlatformRuntime, ResolvedRobot, ResolvedTool,
    discover_robot_yaml, load_robot_with_extras, resolve,
};
use crate::supervisor::{START_LIMIT_BURST, START_LIMIT_INTERVAL};
use crate::utils::{cargo_binary_name, hash_tree, make_executable};

const OPT_ROOT: &str = "/opt/phoxal";
const OPT_BIN: &str = "/opt/phoxal/bin";
const OPT_ENV: &str = "/opt/phoxal/env";
const SYSTEMD_DIR: &str = "/etc/systemd/system";
const IDENTITY_DIR: &str = "/var/lib/phoxal/identity";
const HELPER_PATH: &str = "/usr/local/sbin/phoxal-systemd-helper";
const SUDOERS_PATH: &str = "/etc/sudoers.d/phoxal-deploy";
const RELEASE_SCHEMA: &str = "phoxal.release/v0";
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
        help = "Render, validate, and cross-build without contacting a host."
    )]
    pub dry_run: bool,
    #[arg(
        long,
        value_name = "ARCH",
        help = "Dry-run target arch: aarch64 or x86_64. mender/rauc are reserved; compose/balena are unsupported."
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
        value_enum,
        default_value_t = MessageFormat::Human,
        help = "Output format for the deploy report."
    )]
    pub message_format: MessageFormat,
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
    pub message_format: MessageFormat,
    pub health_timeout: Duration,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DeployReport {
    pub mode: &'static str,
    pub target_arch: String,
    pub official_target_triple: String,
    pub local_target_triple: String,
    pub target_generation: String,
    pub payload_root: PathBuf,
    pub install_plan: InstallPlan,
    pub rendered_units: BTreeMap<String, String>,
    pub env_files: BTreeMap<String, String>,
    pub release_json: Value,
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
}

pub(crate) trait DeployTransport {
    fn probe(&mut self) -> Result<RemoteProbe>;
    fn bootstrap(&mut self, helper: &BootstrapScripts) -> Result<()>;
    fn list_installed_units(&mut self) -> Result<Vec<String>>;
    fn sync_payload(&mut self, payload: &RenderedPayload) -> Result<()>;
    fn install_units(&mut self, payload: &RenderedPayload, stale_units: &[String]) -> Result<()>;
    fn restart(&mut self) -> Result<()>;
    fn health_report(&mut self, units: &[String], deadline: Duration) -> Result<HealthReport>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BootstrapScripts {
    pub helper_script: String,
    pub sudoers_fragment: String,
}

#[derive(Debug)]
pub(crate) struct RenderedPayload {
    pub root: TempDir,
    pub target: TargetTriples,
    pub target_generation: String,
    pub install_plan: InstallPlan,
    pub rendered_units: BTreeMap<String, String>,
    pub env_files: BTreeMap<String, String>,
    pub release_json: Value,
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
    install_binary_name: String,
    source_path: Option<PathBuf>,
    missing_label: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct ReleaseRecord {
    schema: String,
    created_at_utc: String,
    target_generation: String,
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
}

impl Deploy {
    pub async fn run(&self, app: &AppContext) -> Result<()> {
        let options = DeployOptions {
            host: self.host.clone(),
            dry_run: self.dry_run,
            target: self.target.clone(),
            overlays: self.env.clone(),
            catalog_source: app.catalog_source.clone(),
            message_format: self.message_format,
            health_timeout: Duration::from_secs(self.health_timeout_sec),
        };
        let project_root = app.project.root().to_path_buf();
        let ui = app.ui;
        let result = tokio::task::spawn_blocking(move || run(&project_root, options, &ui))
            .await
            .context("deploy worker failed")??;
        report(result, self.message_format)
    }
}

pub(crate) fn run(
    project_start: &Path,
    options: DeployOptions,
    ui: &crate::Ui,
) -> Result<DeployReport> {
    validate_deploy_options(&options)?;
    if options.dry_run {
        let target = target_from_selector(
            options
                .target
                .as_deref()
                .context("--dry-run requires --target <arch>")?,
        )?;
        let payload = prepare_deploy(project_start, &options, target, false, ui)?;
        return Ok(report_from_payload("dry-run", payload, None));
    }

    let host = options
        .host
        .as_deref()
        .context("deploy requires <user@host> unless --dry-run is set")?;
    let mut transport = SshTransport::new(host.to_string(), *ui);
    deploy_with_transport(project_start, &options, &mut transport, ui)
}

pub(crate) fn deploy_with_transport<T: DeployTransport>(
    project_start: &Path,
    options: &DeployOptions,
    transport: &mut T,
    ui: &crate::Ui,
) -> Result<DeployReport> {
    validate_deploy_options(options)?;
    let probe = transport.probe().context("failed to probe deploy host")?;
    let target = target_from_uname_arch(&probe.arch)?;
    let mut payload = prepare_deploy(project_start, options, target, true, ui)?;

    if probe.bootstrap_required {
        transport
            .bootstrap(&payload.bootstrap)
            .context("failed to bootstrap remote phoxal install")?;
    }
    let installed = transport
        .list_installed_units()
        .context("failed to list installed phoxal units")?;
    let stale = stale_units(&installed, &payload.unit_names);
    payload.install_plan.stale_units_to_remove = stale.clone();

    transport
        .sync_payload(&payload)
        .context("failed to sync phoxal payload")?;
    transport
        .install_units(&payload, &stale)
        .context("failed to install systemd units")?;
    transport
        .restart()
        .context("failed to restart phoxal.target")?;
    let health = transport
        .health_report(&payload.unit_names, options.health_timeout)
        .context("failed to collect deploy health")?;
    if !health.is_ok() {
        bail!("{}", format_health_failure(&health));
    }
    Ok(report_from_payload("deploy", payload, Some(health)))
}

fn validate_deploy_options(options: &DeployOptions) -> Result<()> {
    if options.dry_run {
        if options.host.is_some() {
            bail!("--dry-run is hostless; omit <user@host>");
        }
        if options.target.is_none() {
            bail!("--dry-run requires --target <arch>");
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

fn prepare_deploy(
    project_start: &Path,
    options: &DeployOptions,
    target: TargetTriples,
    require_official_binaries: bool,
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
        &loaded.extras,
        false,
    )?;
    let resolved = resolve(
        &loaded.robot,
        project_root,
        catalog.as_ref(),
        ResolveOptions {
            resolve_source_commits: true,
            official_target_triple: Some(target.official_triple.clone()),
            tool_target_triple: Some(target.official_triple.clone()),
        },
    )?;

    let all_source_participants =
        source_participants_from_resolved(project_root, &resolved, component_crate_dir)?;
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
    ensure_no_native_c_source_dependencies(&checked_source_participants)?;
    let robot_graph = robot_graph_from_resolved(&resolved);
    let platform_refs = platform_artifact_refs_from_resolved(&resolved);
    let official_by_ref = resolved
        .platform_runtimes
        .iter()
        .map(|runtime| (runtime.artifact_ref().to_string(), runtime))
        .collect::<BTreeMap<_, _>>();
    let outcome = run_check_with_context(
        &platform_refs,
        &[],
        &checked_source_participants,
        CheckGraphContext {
            robot_graph: &robot_graph,
            manifest_extras: &loaded.extras,
        },
        |artifact_ref| {
            let runtime = official_by_ref.get(artifact_ref).ok_or_else(|| {
                anyhow!("resolved official artifact {artifact_ref} is not in the catalog")
            })?;
            fetch_emit_apis_from_native_artifact(runtime)
        },
        |_| unreachable!("deploy does not check site tools as graph participants"),
        build_emit_apis_from_source,
    )?;
    crate::commands::check::ensure_check_outcome_ok(
        &resolved.target_generation,
        &resolved.channel.to_string(),
        &outcome,
    )?;

    let plan = build_launch_plan(
        LaunchMode::Deploy,
        &[CheckedRobotLaunchInput {
            project_root,
            resolved: &resolved,
            manifest_extras: &loaded.extras,
            checked_participants: &outcome.checked_participants,
            accepted_substitutions: &[],
            source_participants: &checked_source_participants,
        }],
    )?;
    render_payload(RenderPayloadInput {
        project_root,
        robot: &loaded.robot,
        resolved: &resolved,
        plan: &plan,
        source_participants: &all_source_participants,
        target,
        health_timeout: options.health_timeout,
        require_official_binaries,
        ui,
    })
}

struct RenderPayloadInput<'a> {
    project_root: &'a Path,
    robot: &'a Robot,
    resolved: &'a ResolvedRobot,
    plan: &'a LaunchPlan,
    source_participants: &'a [SourceParticipant],
    target: TargetTriples,
    health_timeout: Duration,
    require_official_binaries: bool,
    ui: &'a crate::Ui,
}

fn render_payload(input: RenderPayloadInput<'_>) -> Result<RenderedPayload> {
    let RenderPayloadInput {
        project_root,
        robot,
        resolved,
        plan,
        source_participants,
        target,
        health_timeout,
        require_official_binaries,
        ui,
    } = input;
    let root = tempfile::tempdir().context("failed to create deploy payload directory")?;
    create_payload_dirs(root.path())?;

    let bootstrap = BootstrapScripts {
        helper_script: helper_script(),
        sudoers_fragment: sudoers_fragment(),
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
    let official_plans =
        stage_official_artifacts(root.path(), resolved, plan, require_official_binaries)?;

    let identity_files = identity_install_plan(project_root, robot)?;
    let mut env_files = BTreeMap::new();
    render_env_files(root.path(), plan, &identity_files, &mut env_files)?;

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
    let install_plan = InstallPlan {
        helper_path: HELPER_PATH.to_string(),
        sudoers_path: SUDOERS_PATH.to_string(),
        scoped_delete: vec![format!("{OPT_BIN}/"), format!("{OPT_ENV}/")],
        direct_writes: vec![
            format!("{OPT_ROOT}/robot.yaml"),
            format!("{OPT_ROOT}/phoxal-release.json"),
        ],
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
        target_generation: resolved.target_generation.clone(),
        install_plan,
        rendered_units,
        env_files,
        release_json,
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
    root.join("etc/systemd/system")
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
    let router_source = router_source_participant(source_participants, resolved);
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

    if let Some(participant) = router_source {
        let artifact_id = source_artifact_id(participant);
        if let std::collections::btree_map::Entry::Vacant(entry) = artifacts.entry(artifact_id) {
            let artifact = build_source_artifact(
                project_root,
                root,
                resolved,
                participant,
                entry.key(),
                target,
                ui,
            )?;
            entry.insert(artifact);
        }
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

fn router_source_participant<'a>(
    source_participants: &'a [SourceParticipant],
    resolved: &ResolvedRobot,
) -> Option<&'a SourceParticipant> {
    let router = resolved
        .tools
        .iter()
        .find(|tool| tool.name == SITE_TOOL_ROUTER && tool.path_override.is_some())?;
    source_participants.iter().find(|participant| {
        participant.kind == SourceParticipantKind::Tool && participant.name == router.name
    })
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
    match kind {
        SourceParticipantKind::UserService | SourceParticipantKind::OfficialService => {
            ArtifactKind::Service
        }
        SourceParticipantKind::ComponentDriver => ArtifactKind::Driver,
        SourceParticipantKind::Tool => ArtifactKind::Tool,
        SourceParticipantKind::Simulator => ArtifactKind::Simulator,
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
    {
        return match &component.source {
            ResolvedComponentSource::Git { git, commit, .. } => {
                Ok(serde_json::json!({ "git": git, "rev": commit }))
            }
            ResolvedComponentSource::Path { path } => {
                let full = crate::utils::resolve_project_path(project_root, path);
                Ok(serde_json::json!({
                    "path": path.display().to_string(),
                    "tree": format!("sha256:{}", hash_tree(&full)?)
                }))
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
    let installed = crate::shell::run_stdout("rustup", ["target", "list", "--installed"], None)
        .context("CrossBuildUnsupported: rustup is required to manage deploy cross targets")?;
    if installed.lines().any(|line| line.trim() == target) {
        return Ok(());
    }
    ui.info(format!("provisioning Rust target {target} with rustup"));
    let status = Command::new("rustup")
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
    let tool_root = crate::host_paths::cache_dir()?.join("deploy/tools/zigbuild");
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
    Command::new(program)
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
        let output = Command::new("curl")
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
        let output = Command::new("tar")
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
    let output = Command::new(cargo)
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
    let target_dir = crate::host_paths::cache_dir()?
        .join("deploy/target")
        .join(target);
    ui.info(format!(
        "cross-building {preferred_name} for {target} with cargo zigbuild --release"
    ));
    let mut command = Command::new("cargo");
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
    let status = Command::new("cargo")
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

fn stage_official_artifacts(
    root: &Path,
    resolved: &ResolvedRobot,
    plan: &LaunchPlan,
    require_binaries: bool,
) -> Result<BTreeMap<String, OfficialArtifactPlan>> {
    let mut artifacts = BTreeMap::new();
    for runtime in &resolved.platform_runtimes {
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
        let plan = official_runtime_plan(root, runtime)?;
        if require_binaries && plan.source_path.is_none() {
            bail!(
                "NativePending: official artifact {} is not available locally for {}; run `phoxal-cli pull`, set PHOXAL_ARTIFACT_{}_PATH, or set PHOXAL_ARTIFACT_DIR",
                runtime.artifact_id,
                resolved.target,
                env_key(&runtime.artifact_id)
            );
        }
        artifacts.insert(runtime.artifact_id.clone(), plan);
    }
    let router = resolved
        .tools
        .iter()
        .find(|tool| tool.name == SITE_TOOL_ROUTER)
        .context("resolved deploy graph is missing tool-router")?;
    if router.path_override.is_none() {
        let plan = official_tool_plan(root, router)?;
        if require_binaries && plan.source_path.is_none() {
            bail!(
                "NativePending: official artifact tool-router is not available locally for deploy; run `phoxal-cli pull`, set PHOXAL_ARTIFACT_TOOL_ROUTER_PATH, set PHOXAL_ARTIFACT_DIR, set PHOXAL_TOOL_ROUTER_PATH, or set PHOXAL_TOOL_DIR"
            );
        }
        artifacts.insert(router.name.clone(), plan);
    }
    Ok(artifacts)
}

fn official_runtime_plan(
    root: &Path,
    runtime: &ResolvedPlatformRuntime,
) -> Result<OfficialArtifactPlan> {
    let source_path = locate_official_runtime_binary(runtime)?;
    if let Some(source) = &source_path {
        let dest = payload_bin(root).join(&runtime.artifact_id);
        fs::copy(source, &dest).with_context(|| {
            format!(
                "failed to stage official artifact {} from {}",
                runtime.artifact_id,
                source.display()
            )
        })?;
        make_executable(&dest)?;
    }
    let sha256 = source_path
        .as_deref()
        .map(sha256_file)
        .transpose()?
        .or_else(|| runtime.sha256.clone())
        .unwrap_or_else(|| "0".repeat(64));
    Ok(OfficialArtifactPlan {
        artifact_id: runtime.artifact_id.clone(),
        kind: runtime.kind,
        version: runtime.version.clone(),
        sha256,
        install_binary_name: runtime.artifact_id.clone(),
        source_path,
        missing_label: (runtime.target_status != Some(ArtifactStatus::Released)).then(|| {
            format!(
                "{} ({})",
                runtime.artifact_id,
                runtime
                    .target_status
                    .map(|status| status.to_string())
                    .unwrap_or_else(|| "missing".to_string())
            )
        }),
    })
}

fn official_tool_plan(root: &Path, tool: &ResolvedTool) -> Result<OfficialArtifactPlan> {
    let source_path = locate_tool_binary(tool)?;
    #[cfg(test)]
    let source_path = match source_path {
        Some(path) => Some(path),
        None => Some(test_official_stub(root, &tool.name)?),
    };
    if let Some(source) = &source_path {
        let dest = payload_bin(root).join(&tool.name);
        fs::copy(source, &dest).with_context(|| {
            format!(
                "failed to stage official tool {} from {}",
                tool.name,
                source.display()
            )
        })?;
        make_executable(&dest)?;
    }
    let sha256 = source_path
        .as_deref()
        .map(sha256_file)
        .transpose()?
        .unwrap_or_else(|| tool.sha256.clone());
    Ok(OfficialArtifactPlan {
        artifact_id: tool.name.clone(),
        kind: ArtifactKind::Tool,
        version: tool.resolved.clone(),
        sha256,
        install_binary_name: tool.name.clone(),
        source_path,
        missing_label: None,
    })
}

#[cfg(test)]
fn test_official_stub(root: &Path, name: &str) -> Result<PathBuf> {
    let path = root.join("_test-official").join(name);
    write_text(&path, "#!/bin/sh\nexit 0\n")?;
    make_executable(&path)?;
    Ok(path)
}

fn locate_official_runtime_binary(runtime: &ResolvedPlatformRuntime) -> Result<Option<PathBuf>> {
    if let Some(path) = env_path_override("PHOXAL_ARTIFACT", &runtime.artifact_id) {
        return Ok(Some(path));
    }
    if let Ok(dir) = std::env::var("PHOXAL_ARTIFACT_DIR") {
        for name in [
            runtime.artifact_id.as_str(),
            &crate::resolver::official_binary_name(runtime.kind, &runtime.name),
        ] {
            let path = PathBuf::from(&dir).join(name);
            if path.is_file() {
                return Ok(Some(path));
            }
        }
    }
    let Some(descriptor) =
        crate::native_artifacts::NativeArtifactDescriptor::from_runtime(runtime)?
    else {
        return Ok(None);
    };
    let cache = crate::native_artifacts::artifact_binary_path(&descriptor)?;
    Ok(cache.is_file().then_some(cache))
}

fn locate_tool_binary(tool: &ResolvedTool) -> Result<Option<PathBuf>> {
    if let Some(path) = env_path_override("PHOXAL_ARTIFACT", &tool.name) {
        return Ok(Some(path));
    }
    if let Some(path) = env_path_override("PHOXAL_TOOL", &tool.name) {
        return Ok(Some(path));
    }
    if let Ok(dir) = std::env::var("PHOXAL_ARTIFACT_DIR") {
        let path = PathBuf::from(&dir).join(&tool.binary_name);
        if path.is_file() {
            return Ok(Some(path));
        }
    }
    if let Ok(dir) = std::env::var("PHOXAL_TOOL_DIR") {
        let path = PathBuf::from(&dir).join(&tool.name);
        if path.is_file() {
            return Ok(Some(path));
        }
        let path = PathBuf::from(&dir).join(&tool.binary_name);
        if path.is_file() {
            return Ok(Some(path));
        }
    }
    let Some(descriptor) = crate::native_artifacts::NativeArtifactDescriptor::from_tool(tool)?
    else {
        return Ok(None);
    };
    let cache = crate::native_artifacts::artifact_binary_path(&descriptor)?;
    Ok(cache.is_file().then_some(cache))
}

fn env_path_override(prefix: &str, id: &str) -> Option<PathBuf> {
    let key = format!("{prefix}_{}_PATH", env_key(id));
    std::env::var_os(key)
        .map(PathBuf::from)
        .filter(|path| path.is_file())
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

fn identity_install_plan(project_root: &Path, robot: &Robot) -> Result<Vec<IdentityInstallPlan>> {
    let Some(uplink) = &robot.bus.uplink else {
        return Ok(Vec::new());
    };
    let Some(auth) = &uplink.auth else {
        return Ok(Vec::new());
    };
    let files = [
        (&auth.ca, "uplink-ca.pem"),
        (&auth.cert, "uplink-client.pem"),
        (&auth.key, "uplink-client.key"),
    ];
    files
        .into_iter()
        .map(|(local, name)| {
            let local_path = crate::utils::resolve_project_path(project_root, local);
            if !local_path.is_file() {
                bail!(
                    "bus.uplink.auth file {} does not exist",
                    local_path.display()
                );
            }
            Ok(IdentityInstallPlan {
                local_path,
                remote_path: format!("{IDENTITY_DIR}/{name}"),
            })
        })
        .collect()
}

fn render_env_files(
    root: &Path,
    plan: &LaunchPlan,
    identity_files: &[IdentityInstallPlan],
    env_files: &mut BTreeMap<String, String>,
) -> Result<()> {
    let robot = plan
        .robots
        .first()
        .context("deploy launch plan has no robot")?;
    let router = plan
        .site
        .iter()
        .find(|site| site.id == SITE_TOOL_ROUTER)
        .context("deploy launch plan has no tool-router")?;
    let router_env = router_env(router, &robot.namespace, &robot.id, identity_files)?;
    write_env_file(root, "router.env", &router_env, env_files)?;

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

fn router_env(
    site: &SiteLaunch,
    namespace: &str,
    robot_id: &str,
    identity_files: &[IdentityInstallPlan],
) -> Result<EncodedParticipantEnv> {
    let mut variables = BTreeMap::new();
    variables.insert(env::PARTICIPANT_ID.to_string(), site.id.clone());
    variables.insert(env::NAMESPACE.to_string(), namespace.to_string());
    variables.insert(env::ROBOT_ID.to_string(), robot_id.to_string());
    variables.insert(env::ROBOT_ROOT.to_string(), OPT_ROOT.to_string());
    variables.insert(
        env::CONFIG.to_string(),
        serde_json::to_string(&router_config_with_identity_paths(
            &site.phoxal_config,
            identity_files,
        )?)
        .with_context(|| format!("failed to encode PHOXAL_CONFIG for {}", site.id))?,
    );
    variables.insert(env::CLOCK.to_string(), "real".to_string());
    Ok(EncodedParticipantEnv::from_variables(variables))
}

fn router_config_with_identity_paths(
    config: &Value,
    identity_files: &[IdentityInstallPlan],
) -> Result<Value> {
    let mut config = config.clone();
    if identity_files.is_empty() {
        return Ok(config);
    }
    let Value::Object(root) = &mut config else {
        return Ok(config);
    };
    let Some(Value::Object(uplink)) = root.get_mut("uplink") else {
        return Ok(config);
    };
    let mut auth = Map::new();
    for file in identity_files {
        if file.remote_path.ends_with("uplink-ca.pem") {
            auth.insert("ca".to_string(), Value::String(file.remote_path.clone()));
        } else if file.remote_path.ends_with("uplink-client.pem") {
            auth.insert("cert".to_string(), Value::String(file.remote_path.clone()));
        } else if file.remote_path.ends_with("uplink-client.key") {
            auth.insert("key".to_string(), Value::String(file.remote_path.clone()));
        }
    }
    uplink.insert("auth".to_string(), Value::Object(auth));
    Ok(config)
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

    let router_binary = if source_builds.contains_key(SITE_TOOL_ROUTER) {
        SITE_TOOL_ROUTER.to_string()
    } else {
        official_plans
            .get(SITE_TOOL_ROUTER)
            .map(|tool| tool.install_binary_name.clone())
            .unwrap_or_else(|| SITE_TOOL_ROUTER.to_string())
    };
    write_unit(
        root,
        "phoxal-router.service",
        &router_unit(&router_binary),
        rendered_units,
    )?;
    unit_names.push("phoxal-router.service".to_string());

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
        "[Unit]\nDescription=Phoxal Zenoh router\nAfter=network-online.target\nWants=network-online.target\nPartOf=phoxal.target\n\n[Service]\nType=notify\nEnvironmentFile={OPT_ENV}/router.env\nExecStart={OPT_BIN}/{binary}\nRestart=on-failure\nRestartSec=2s\nStartLimitIntervalSec={}\nStartLimitBurst={START_LIMIT_BURST}\nWatchdogSec={WATCHDOG_SEC}s\nUser=phoxal\nGroup=phoxal\nNoNewPrivileges=true\n\n[Install]\nWantedBy=phoxal.target\n",
        START_LIMIT_INTERVAL.as_secs()
    )
}

fn participant_unit(
    participant: &ParticipantLaunchRecord,
    binary: &str,
    privileges: &UnitPrivileges,
) -> String {
    let id = &participant.launch.participant_id;
    let mut unit = format!(
        "[Unit]\nDescription=Phoxal participant {id}\nAfter=network-online.target phoxal-router.service\nWants=network-online.target\nPartOf=phoxal.target\n\n[Service]\nType=notify\nEnvironmentFile={OPT_ENV}/{id}.env\nExecStart={OPT_BIN}/{binary}\n\nRestart=on-failure\nRestartSec=2s\nStartLimitIntervalSec={}\nStartLimitBurst={START_LIMIT_BURST}\nTimeoutStopSec=5s\nStateDirectory=phoxal\nWatchdogSec={WATCHDOG_SEC}s\n\nUser=phoxal\nGroup=phoxal\nNoNewPrivileges=true\n",
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
            let runtime = resolved
                .platform_runtimes
                .iter()
                .find(|runtime| runtime.name == participant.artifact_id)
                .ok_or_else(|| {
                    anyhow!(
                        "official participant {} has no resolved runtime",
                        participant.artifact_id
                    )
                })?;
            official_plans
                .get(&runtime.artifact_id)
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
    let Some(component) = resolved.robot.components.instances.get(participant_id) else {
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
    let yaml = serde_yaml::to_string(robot).context("failed to serialize resolved robot.yaml")?;
    write_text(&payload_opt(root).join("robot.yaml"), &yaml)
}

fn release_record(
    resolved: &ResolvedRobot,
    plan: &LaunchPlan,
    source_builds: &BTreeMap<String, SourceBuildArtifact>,
    official_plans: &BTreeMap<String, OfficialArtifactPlan>,
) -> Result<ReleaseRecord> {
    let mut artifacts = BTreeMap::<String, ReleaseArtifact>::new();
    if let Some(router) = source_builds.get(SITE_TOOL_ROUTER) {
        artifacts.insert(router.artifact_id.clone(), release_source_artifact(router));
    } else if let Some(router) = official_plans.get(SITE_TOOL_ROUTER) {
        artifacts.insert(
            router.artifact_id.clone(),
            release_official_artifact(router),
        );
    }

    for participant in &plan.robots[0].participants {
        match &participant.execution {
            ParticipantExecution::OfficialArtifact { .. } => {
                let runtime = resolved
                    .platform_runtimes
                    .iter()
                    .find(|runtime| runtime.name == participant.artifact_id)
                    .ok_or_else(|| anyhow!("missing runtime for {}", participant.artifact_id))?;
                if let Some(artifact) = official_plans.get(&runtime.artifact_id) {
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
        target_generation: resolved.target_generation.clone(),
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
    }
}

fn release_official_artifact(artifact: &OfficialArtifactPlan) -> ReleaseArtifact {
    ReleaseArtifact {
        id: artifact.artifact_id.clone(),
        kind: artifact.kind.to_string(),
        version: Some(artifact.version.clone()),
        source: Value::String("catalog".to_string()),
        sha256: artifact.sha256.clone(),
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

valid_unit() {{
  case "$1" in
    phoxal.target|phoxal-router.service|phoxal-participant-*.service) ;;
    *) return 1 ;;
  esac
  case "$1" in
    *[!A-Za-z0-9_.@-]*) return 1 ;;
  esac
  return 0
}}

case "${{1:-}}" in
  install-unit)
    unit="${{2:-}}"
    source="${{3:-}}"
    valid_unit "$unit"
    case "$source" in /tmp/phoxal-units-*/*) ;; *) exit 64 ;; esac
    test -f "$source"
    install -o root -g root -m 0644 "$source" "$unit_dir/$unit"
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
    systemctl restart phoxal.target
    ;;
  *)
    exit 64
    ;;
esac
"#
    )
}

fn sudoers_fragment() -> String {
    format!("phoxal ALL=(root) NOPASSWD: {HELPER_PATH} *\n")
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
install -d -o phoxal -g phoxal -m 0755 {OPT_ROOT} {OPT_BIN} {OPT_ENV}
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
        target_generation: payload.target_generation,
        payload_root: payload.root.path().to_path_buf(),
        install_plan: payload.install_plan,
        rendered_units: payload.rendered_units,
        env_files: payload.env_files,
        release_json: payload.release_json,
        health,
    }
}

fn report(report: DeployReport, message_format: MessageFormat) -> Result<()> {
    crate::commands::print_message(
        &report,
        || {
            println!("mode: {}", report.mode);
            println!("target_arch: {}", report.target_arch);
            println!("official_target: {}", report.official_target_triple);
            println!("local_target: {}", report.local_target_triple);
            println!("target_generation: {}", report.target_generation);
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
            if let Some(health) = &report.health {
                println!("health:");
                println!("{}", serde_json::to_string_pretty(health)?);
            }
            Ok(())
        },
        message_format,
    )
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

#[derive(Debug)]
struct SshTransport {
    host: String,
    ui: crate::Ui,
}

impl SshTransport {
    fn new(host: String, ui: crate::Ui) -> Self {
        Self { host, ui }
    }

    fn ssh_output<I, S>(&self, args: I) -> Result<std::process::Output>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut command = Command::new("ssh");
        command.arg(&self.host).args(args);
        command
            .output()
            .with_context(|| format!("failed to run ssh {}", self.host))
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
        let mut command = Command::new("rsync");
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
}

impl DeployTransport for SshTransport {
    fn probe(&mut self) -> Result<RemoteProbe> {
        let arch = self.ssh_stdout(["uname", "-m"])?.trim().to_string();
        let bootstrap_required = self
            .ssh_output(["test", "-d", OPT_ROOT])
            .map(|output| !output.status.success())
            .unwrap_or(true);
        Ok(RemoteProbe {
            arch,
            bootstrap_required,
        })
    }

    fn bootstrap(&mut self, helper: &BootstrapScripts) -> Result<()> {
        let script = bootstrap_script(helper);
        let mut child = Command::new("ssh")
            .arg(&self.host)
            .arg("sudo")
            .arg("sh")
            .arg("-s")
            .stdin(Stdio::piped())
            .spawn()
            .with_context(|| format!("failed to start bootstrap ssh {}", self.host))?;
        child
            .stdin
            .as_mut()
            .context("bootstrap child stdin was not available")?
            .write_all(script.as_bytes())
            .context("failed to write bootstrap script")?;
        let status = child.wait().context("failed to wait for bootstrap ssh")?;
        if status.success() {
            Ok(())
        } else {
            bail!("remote bootstrap failed with status {status}")
        }
    }

    fn list_installed_units(&mut self) -> Result<Vec<String>> {
        let output = self.ssh_stdout([
            "systemctl",
            "list-unit-files",
            "phoxal*",
            "--no-legend",
            "--no-pager",
        ])?;
        Ok(output
            .lines()
            .filter_map(|line| line.split_whitespace().next().map(str::to_string))
            .filter(|unit| managed_unit_name(unit))
            .collect())
    }

    fn sync_payload(&mut self, payload: &RenderedPayload) -> Result<()> {
        self.rsync(vec![
            OsString::from("-az"),
            OsString::from("--delete"),
            payload_bin(payload.root.path()).join("").into_os_string(),
            OsString::from(format!("{}:{OPT_BIN}/", self.host)),
        ])?;
        self.rsync(vec![
            OsString::from("-az"),
            OsString::from("--delete"),
            payload_env(payload.root.path()).join("").into_os_string(),
            OsString::from(format!("{}:{OPT_ENV}/", self.host)),
        ])?;
        self.rsync(vec![
            OsString::from("-az"),
            payload_opt(payload.root.path())
                .join("robot.yaml")
                .into_os_string(),
            OsString::from(format!("{}:{OPT_ROOT}/robot.yaml", self.host)),
        ])?;
        self.rsync(vec![
            OsString::from("-az"),
            payload_opt(payload.root.path())
                .join("phoxal-release.json")
                .into_os_string(),
            OsString::from(format!("{}:{OPT_ROOT}/phoxal-release.json", self.host)),
        ])?;
        if !payload.install_plan.identity_files.is_empty() {
            self.ssh_status(["install", "-d", "-m", "0700", IDENTITY_DIR])?;
        }
        for identity in &payload.install_plan.identity_files {
            self.rsync(vec![
                OsString::from("-az"),
                identity.local_path.clone().into_os_string(),
                OsString::from(format!("{}:{}", self.host, identity.remote_path)),
            ])?;
            self.ssh_status(["chmod", "0600", &identity.remote_path])?;
        }
        Ok(())
    }

    fn install_units(&mut self, payload: &RenderedPayload, stale_units: &[String]) -> Result<()> {
        let remote_tmp = format!("/tmp/phoxal-units-{}", std::process::id());
        self.ssh_status(["rm", "-rf", &remote_tmp])?;
        self.ssh_status(["mkdir", "-p", &remote_tmp])?;
        self.rsync(vec![
            OsString::from("-az"),
            payload_systemd(payload.root.path())
                .join("")
                .into_os_string(),
            OsString::from(format!("{}:{remote_tmp}/", self.host)),
        ])?;
        for unit in &payload.unit_names {
            let remote_unit_path = format!("{remote_tmp}/{unit}");
            self.run_helper(&["install-unit", unit, &remote_unit_path])?;
        }
        for unit in stale_units {
            self.run_helper(&["disable-unit", unit])?;
            self.run_helper(&["remove-unit", unit])?;
        }
        self.run_helper(&["daemon-reload"])?;
        for unit in &payload.unit_names {
            self.run_helper(&["enable-unit", unit])?;
        }
        self.ssh_status(["rm", "-rf", &remote_tmp])?;
        Ok(())
    }

    fn restart(&mut self) -> Result<()> {
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
        .or_else(|| (unit == "phoxal-router.service").then(|| SITE_TOOL_ROUTER.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use phoxal_cli_test_support::write_basic_project;

    mod phoxal_cli_test_support {
        use super::*;
        use crate::catalog::{
            ArtifactStatus, Channel as CatalogChannel, fixture_catalog_for_tests,
            fixture_tool_entry_for_tests,
        };

        pub fn write_basic_project(root: &Path) -> Result<()> {
            fs::write(root.join("robot.yaml"), basic_robot_yaml())?;
            write_catalog(root)?;
            write_service_crate(root, "mission", "service", "mission")?;
            Ok(())
        }

        pub fn write_driver_project(root: &Path) -> Result<()> {
            fs::write(root.join("robot.yaml"), driver_robot_yaml())?;
            write_catalog(root)?;
            write_service_crate(root, "mission", "service", "mission")?;
            write_driver_crate(root, "ddsm115", "driver-ddsm115")?;
            Ok(())
        }

        pub fn write_native_dep_project(root: &Path) -> Result<()> {
            fs::write(root.join("robot.yaml"), basic_robot_yaml())?;
            write_catalog(root)?;
            let dir = root.join("runtimes/mission");
            fs::create_dir_all(dir.join("src"))?;
            fs::write(
                dir.join("Cargo.toml"),
                "[package]\nname = \"mission\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\nopencv = \"0.1\"\n",
            )?;
            fs::write(dir.join("src/main.rs"), service_main("service", "mission"))?;
            Ok(())
        }

        fn write_catalog(root: &Path) -> Result<()> {
            let catalog = fixture_catalog_for_tests(vec![fixture_tool_entry_for_tests(
                "router",
                "y2026_1",
                "0.1.0",
                CatalogChannel::Stable,
                "aarch64-unknown-linux-gnu",
                ArtifactStatus::Pending,
                Vec::new(),
            )]);
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

        fn service_main(kind: &str, artifact_id: &str) -> String {
            format!(
                "fn main() {{\n    if std::env::args().nth(1).as_deref() == Some(\"emit-apis\") {{\n        println!(\"{{}}\", r#\"{{\"artifact\":{{\"kind\":\"{kind}\",\"id\":\"{artifact_id}\"}},\"participant_class\":\"checked\",\"api_version\":\"source\",\"required_contracts\":[]}}\"#);\n    }}\n}}\n"
            )
        }

        fn basic_robot_yaml() -> &'static str {
            r#"schema: v0
identity:
  id: testbot
  namespace: dev
structure: structure.urdf
phoxal_artifacts:
  channel: stable
  generation: y2026_1
  catalog: catalog.json
phoxal_participants: {}
user_participants:
  mission:
    path: runtimes/mission
motion:
  kinematic:
    kind: differential
    left_actuators: [left_drive.motor]
    right_actuators: [right_drive.motor]
    left_encoders: [left_drive.encoder]
    right_encoders: [right_drive.encoder]
    wheel_radius_m: 0.1
    wheel_base_m: 0.5
components:
  sources:
    ddsm115:
      path: components/ddsm115
  instances:
    left_drive:
      component: ddsm115
      mount_link: left_wheel
    right_drive:
      component: ddsm115
      mount_link: right_wheel
"#
        }

        fn driver_robot_yaml() -> &'static str {
            r#"schema: v0
identity:
  id: testbot
  namespace: dev
structure: structure.urdf
phoxal_artifacts:
  channel: stable
  generation: y2026_1
  catalog: catalog.json
phoxal_participants: {}
user_participants:
  mission:
    path: runtimes/mission
motion:
  kinematic:
    kind: differential
    left_actuators: [left_drive.motor]
    right_actuators: [right_drive.motor]
    left_encoders: [left_drive.encoder]
    right_encoders: [right_drive.encoder]
    wheel_radius_m: 0.1
    wheel_base_m: 0.5
components:
  sources:
    ddsm115:
      path: components/ddsm115
  instances:
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
"#
        }
    }

    #[derive(Debug)]
    struct FakeTransport {
        probe: RemoteProbe,
        installed_units: Vec<String>,
        health: HealthReport,
        bootstrapped: bool,
        synced: bool,
        stale_removed: Vec<String>,
        restarted: bool,
    }

    impl FakeTransport {
        fn healthy() -> Self {
            Self {
                probe: RemoteProbe {
                    arch: "aarch64".to_string(),
                    bootstrap_required: true,
                },
                installed_units: Vec::new(),
                health: HealthReport { units: Vec::new() },
                bootstrapped: false,
                synced: false,
                stale_removed: Vec::new(),
                restarted: false,
            }
        }
    }

    impl DeployTransport for FakeTransport {
        fn probe(&mut self) -> Result<RemoteProbe> {
            Ok(self.probe.clone())
        }

        fn bootstrap(&mut self, _helper: &BootstrapScripts) -> Result<()> {
            self.bootstrapped = true;
            Ok(())
        }

        fn list_installed_units(&mut self) -> Result<Vec<String>> {
            Ok(self.installed_units.clone())
        }

        fn sync_payload(&mut self, _payload: &RenderedPayload) -> Result<()> {
            self.synced = true;
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

    fn dry_options() -> DeployOptions {
        DeployOptions {
            host: None,
            dry_run: true,
            target: Some("aarch64".to_string()),
            overlays: Vec::new(),
            catalog_source: None,
            message_format: MessageFormat::Human,
            health_timeout: Duration::from_secs(3),
        }
    }

    fn live_options() -> DeployOptions {
        DeployOptions {
            host: Some("robot@test".to_string()),
            dry_run: false,
            target: None,
            overlays: Vec::new(),
            catalog_source: None,
            message_format: MessageFormat::Human,
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
        let temp = tempfile::tempdir()?;
        write_basic_project(temp.path())?;
        let payload = prepare_deploy(
            temp.path(),
            &dry_options(),
            target_for_arch("aarch64"),
            false,
            &crate::Ui,
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
            .get("/etc/systemd/system/phoxal-participant-mission.service")
            .expect("mission unit rendered");
        assert!(participant_unit.contains("Type=notify"));
        assert!(participant_unit.contains("WatchdogSec=10s"));
        assert!(participant_unit.contains("ExecStart=/opt/phoxal/bin/mission"));
        assert!(
            payload
                .env_files
                .contains_key("/opt/phoxal/env/mission.env")
        );
        assert_eq!(payload.release_json["schema"], RELEASE_SCHEMA);
        assert!(
            payload
                .install_plan
                .scoped_delete
                .contains(&"/opt/phoxal/bin/".to_string())
        );
        assert!(
            payload
                .install_plan
                .units
                .contains(&"phoxal-participant-mission.service".to_string())
        );
        Ok(())
    }

    #[test]
    fn driver_graph_renders_one_unit_per_instance_with_privileges() -> Result<()> {
        let temp = tempfile::tempdir()?;
        phoxal_cli_test_support::write_driver_project(temp.path())?;
        let payload = prepare_deploy(
            temp.path(),
            &dry_options(),
            target_for_arch("aarch64"),
            false,
            &crate::Ui,
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
        assert!(left.contains("ExecStart=/opt/phoxal/bin/driver-ddsm115"));
        assert!(right.contains("ExecStart=/opt/phoxal/bin/driver-ddsm115"));
        Ok(())
    }

    #[test]
    fn privileged_tool_graph_renders_router_only_not_joypad() -> Result<()> {
        let temp = tempfile::tempdir()?;
        write_basic_project(temp.path())?;
        let payload = prepare_deploy(
            temp.path(),
            &dry_options(),
            target_for_arch("aarch64"),
            false,
            &crate::Ui,
        )?;
        assert!(
            payload
                .rendered_units
                .contains_key("/etc/systemd/system/phoxal-router.service")
        );
        assert!(
            !payload
                .rendered_units
                .keys()
                .any(|unit| unit.contains("joypad"))
        );
        Ok(())
    }

    #[test]
    fn rejected_non_immutable_artifact_gets_designed_error() -> Result<()> {
        let temp = tempfile::tempdir()?;
        phoxal_cli_test_support::write_native_dep_project(temp.path())?;
        let error = prepare_deploy(
            temp.path(),
            &dry_options(),
            target_for_arch("aarch64"),
            false,
            &crate::Ui,
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
        let temp = tempfile::tempdir()?;
        write_basic_project(temp.path())?;
        let mut transport = FakeTransport::healthy();
        transport.installed_units = vec![
            "phoxal.target".to_string(),
            "phoxal-router.service".to_string(),
            "phoxal-participant-old.service".to_string(),
        ];
        let report =
            deploy_with_transport(temp.path(), &live_options(), &mut transport, &crate::Ui)?;
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
        Ok(())
    }

    #[cfg(unix)]
    fn write_test_executable(path: &Path, contents: &str) -> Result<()> {
        fs::write(path, contents)?;
        make_executable(path)?;
        Ok(())
    }

    #[test]
    fn failed_health_push_exits_nonzero_with_diagnosis() -> Result<()> {
        let temp = tempfile::tempdir()?;
        write_basic_project(temp.path())?;
        let mut transport = FakeTransport::healthy();
        transport.health = HealthReport {
            units: vec![HealthUnitReport {
                unit: "phoxal-participant-mission.service".to_string(),
                participant: Some("mission".to_string()),
                ready: false,
                active_state: "failed".to_string(),
                sub_state: "failed".to_string(),
                journal_excerpt: vec!["boom".to_string()],
            }],
        };
        let error = deploy_with_transport(temp.path(), &live_options(), &mut transport, &crate::Ui)
            .expect_err("health failure should fail deploy");
        let message = error.to_string();
        assert!(message.contains("HealthReportFailed"), "{message}");
        assert!(message.contains("mission"), "{message}");
        assert!(message.contains("boom"), "{message}");
        Ok(())
    }
}
