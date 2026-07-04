use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use clap::{Args, ValueEnum};
use phoxal::model::robot::v1::ConnectionConfig;
use phoxal::participant::launch::env;
use serde::Serialize;
use serde_json::Value;
use tokio::sync::mpsc;

use crate::AppContext;
use crate::commands::MessageFormat;
use crate::commands::check::{
    CheckGraphContext, build_emit_apis_from_source, fetch_emit_apis_from_native_artifact,
    platform_artifact_refs_from_resolved, robot_graph_from_resolved, run_check_with_context,
    source_participants_from_resolved,
};
use crate::component_driver::component_crate_dir;
use crate::launch_env::encode_participant_env;
use crate::launch_plan::{
    CheckedRobotLaunchInput, LaunchMode, LaunchPlan, ParticipantExecution, ParticipantLaunchRecord,
    SITE_TOOL_JOYPAD, SITE_TOOL_ROUTER, SiteLaunch, build_launch_plan,
};
use crate::resolver::{
    ResolveOptions, ResolvedPlatformRuntime, ResolvedRobot, discover_robot_yaml,
    host_target_triple, load_robot_with_extras, resolve,
};
use crate::supervisor::{
    BoardBackend, ParticipantKind, ParticipantSpec, ParticipantState, ParticipantStatus,
    RouterOwnership, SupervisorLock, SupervisorOptions, default_connect_endpoint,
    local_router_reachable, router_ownership, start_bus_log_subscriber, supervise_until_shutdown,
    supervisor_actions_path, supervisor_state_path,
};
use crate::utils::cargo_binary_name;

#[derive(Debug, Args)]
pub struct Run {
    #[arg(
        long,
        help = "Refresh the catalog and native artifact cache before launch."
    )]
    pub pull: bool,
    #[arg(
        long = "driver",
        value_name = "ID",
        help = "Launch only the named component driver. Repeat for a strict bench subset."
    )]
    pub drivers_subset: Vec<String>,
    #[arg(
        long = "drivers",
        value_enum,
        default_value_t = DriversMode::On,
        help = "Driver launch policy."
    )]
    pub drivers: DriversMode,
    #[arg(
        long,
        help = "Watch local source artifacts and hot-reload checked changes."
    )]
    pub watch: bool,
    #[arg(
        long = "env",
        value_name = "ENV",
        help = "Apply a robot.<env>.yaml overlay before running (repeatable). Path pins are only legal through overlays."
    )]
    pub env: Vec<String>,
    #[arg(long, value_enum, default_value_t = MessageFormat::Human)]
    pub message_format: MessageFormat,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum DriversMode {
    On,
    Off,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunOptions {
    pub pull: bool,
    pub drivers: DriversMode,
    pub drivers_subset: Vec<String>,
    pub catalog_source: Option<String>,
    pub overlays: Vec<String>,
    pub watch: bool,
    pub message_format: MessageFormat,
}

#[derive(Debug)]
struct PreparedRun {
    project_root: PathBuf,
    resolved: ResolvedRobot,
    source_participants: Vec<crate::commands::check::SourceParticipant>,
    plan: LaunchPlan,
    board: BoardBackend,
    specs: Vec<ParticipantSpec>,
    robot_log_targets: Vec<(String, String)>,
    router_ownership: RouterOwnership,
}

impl Run {
    pub async fn run(&self, app: &AppContext) -> Result<()> {
        let options = RunOptions {
            pull: self.pull,
            drivers: self.drivers,
            drivers_subset: self.drivers_subset.clone(),
            catalog_source: app.catalog_source.clone(),
            overlays: self.env.clone(),
            watch: self.watch,
            message_format: self.message_format,
        };
        if options.drivers == DriversMode::Off && !options.drivers_subset.is_empty() {
            bail!("--driver cannot be combined with --drivers off");
        }
        let watch_enabled = options.watch;
        let message_format = options.message_format;
        let watch_options = options.clone();

        let run_dir = crate::host_paths::run_dir()?;
        let _lock = SupervisorLock::acquire(&run_dir)?;
        let state_file = supervisor_state_path()?;
        let action_file = supervisor_actions_path()?;
        let project_root = app.project.root().to_path_buf();
        let ui = app.ui;
        let prepared =
            tokio::task::spawn_blocking(move || prepare_run(&project_root, options, &ui))
                .await
                .context("run preparation worker failed")??;

        app.ui.info(format!(
            "launch plan resolved: {} robot(s), {} site tool(s)",
            prepared.plan.robots.len(),
            prepared.plan.site.len()
        ));
        match prepared.router_ownership {
            RouterOwnership::External => app.ui.info("reusing reachable external tool-router"),
            RouterOwnership::Managed => app.ui.info("tool-router will be managed by this session"),
        }
        report_launch_commands(&prepared.specs, message_format)?;

        let _log_tasks = prepared
            .robot_log_targets
            .iter()
            .map(|(namespace, robot_id)| {
                start_bus_log_subscriber(
                    namespace.clone(),
                    robot_id.clone(),
                    default_connect_endpoint(),
                    prepared.board.clone(),
                )
            })
            .collect::<Vec<_>>();

        let (action_rx, watch_handle) = if watch_enabled {
            let (action_tx, action_rx) = mpsc::channel(16);
            let live_ids = prepared
                .specs
                .iter()
                .map(|spec| spec.id.clone())
                .collect::<BTreeSet<_>>();
            let handle = crate::watch::spawn_run_watch(crate::watch::RunWatchConfig {
                project_root: prepared.project_root.clone(),
                options: watch_options,
                resolved: prepared.resolved.clone(),
                source_participants: prepared.source_participants.clone(),
                live_ids,
                board: prepared.board.clone(),
                action_tx,
            });
            (Some(action_rx), Some(handle))
        } else {
            (None, None)
        };

        let outcome = supervise_until_shutdown(
            prepared.specs,
            prepared.board.clone(),
            SupervisorOptions {
                state_file: Some(state_file),
                action_file: Some(action_file),
                action_rx,
                ..SupervisorOptions::default()
            },
        )
        .await;
        if let Some(handle) = watch_handle {
            handle.abort();
        }
        let outcome = outcome?;

        if !outcome.graph_healthy() {
            bail!(
                "supervisor graph ended unhealthy; failed participants: {}",
                outcome.failed_participants.join(", ")
            );
        }
        Ok(())
    }
}

fn prepare_run(project_start: &Path, options: RunOptions, ui: &crate::Ui) -> Result<PreparedRun> {
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
        options.pull,
    )?;
    let resolved = resolve(
        &loaded.robot,
        project_root,
        catalog.as_ref(),
        ResolveOptions {
            resolve_source_commits: true,
            ..ResolveOptions::default()
        },
    )?;

    if options.pull
        && let Err(error) = crate::native_artifacts::stage_resolved_artifacts(
            Some(ui),
            &resolved,
            crate::native_artifacts::ProvisioningMode::Refresh,
        )
    {
        ui.warn(format!(
            "native artifact refresh failed; using cache/path overrides only: {error:#}"
        ));
    }

    let source_participants =
        source_participants_from_resolved(project_root, &resolved, component_crate_dir)?;
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
        &source_participants,
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
        |_| unreachable!("run does not check site tools as graph participants"),
        build_emit_apis_from_source,
    )?;
    if !outcome.is_ok() {
        crate::commands::check::ensure_check_outcome_ok(
            &resolved.target_generation,
            &resolved.channel.to_string(),
            &outcome,
        )?;
    }
    for warning in &outcome.report.warnings {
        ui.warn(format!("graph warning: {warning:?}"));
    }

    let plan = build_launch_plan(
        LaunchMode::Run,
        &[CheckedRobotLaunchInput {
            project_root,
            resolved: &resolved,
            manifest_extras: &loaded.extras,
            checked_participants: &outcome.checked_participants,
            accepted_substitutions: &[],
            source_participants: &source_participants,
        }],
    )?;

    let driver_policy = DriverPolicy::from_options(&options, &plan)?;
    let board = BoardBackend::new();
    let router_ownership = router_ownership(local_router_reachable(&default_connect_endpoint()));
    let mut specs = Vec::new();

    prepare_site_tools(&plan, &resolved, &board, &mut specs, router_ownership)?;
    prepare_robot_participants(
        &plan,
        &resolved,
        project_root,
        &driver_policy,
        &board,
        &mut specs,
        ui,
    )?;

    Ok(PreparedRun {
        project_root: project_root.to_path_buf(),
        resolved,
        source_participants,
        robot_log_targets: plan
            .robots
            .iter()
            .map(|robot| (robot.namespace.clone(), robot.id.clone()))
            .collect(),
        plan,
        board,
        specs,
        router_ownership,
    })
}

#[derive(Debug, Serialize)]
struct LaunchCommandReport {
    participants: Vec<LaunchCommandEntry>,
}

#[derive(Debug, Serialize)]
struct LaunchCommandEntry {
    id: String,
    kind: &'static str,
    command_line: String,
    env: BTreeMap<String, String>,
}

pub(crate) fn report_launch_commands(
    specs: &[ParticipantSpec],
    message_format: MessageFormat,
) -> Result<()> {
    if message_format != MessageFormat::Json {
        return Ok(());
    }
    let output = LaunchCommandReport {
        participants: specs
            .iter()
            .map(|spec| {
                let launch = spec.launch_command();
                LaunchCommandEntry {
                    id: spec.id.clone(),
                    kind: spec.kind.label(),
                    command_line: launch.command_line,
                    env: launch.env,
                }
            })
            .collect(),
    };
    crate::commands::print_message(&output, || Ok(()), message_format)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DriverPolicy {
    mode: DriversMode,
    subset: BTreeSet<String>,
}

impl DriverPolicy {
    pub(crate) fn drivers_off_for_sim() -> Self {
        Self {
            mode: DriversMode::Off,
            subset: BTreeSet::new(),
        }
    }

    pub(crate) fn from_options(options: &RunOptions, plan: &LaunchPlan) -> Result<Self> {
        let available = plan
            .robots
            .iter()
            .flat_map(|robot| &robot.participants)
            .filter(|participant| {
                matches!(
                    participant.execution,
                    ParticipantExecution::ComponentDriver { .. }
                )
            })
            .map(|participant| participant.launch.participant_id.clone())
            .collect::<BTreeSet<_>>();
        let subset = options
            .drivers_subset
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>();
        let unknown = subset.difference(&available).cloned().collect::<Vec<_>>();
        if !unknown.is_empty() {
            let available = available.into_iter().collect::<Vec<_>>().join(", ");
            bail!(
                "unknown driver id(s): {}; available drivers: {}",
                unknown.join(", "),
                if available.is_empty() {
                    "<none>".to_string()
                } else {
                    available
                }
            );
        }
        Ok(Self {
            mode: options.drivers,
            subset,
        })
    }

    fn decision(&self, id: &str) -> DriverDecision {
        match self.mode {
            DriversMode::Off => DriverDecision::Degraded("drivers off".to_string()),
            DriversMode::On if !self.subset.is_empty() && !self.subset.contains(id) => {
                DriverDecision::Degraded("not selected by --driver".to_string())
            }
            DriversMode::On => DriverDecision::Launch,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum DriverDecision {
    Launch,
    Degraded(String),
}

pub(crate) fn prepare_site_tools(
    plan: &LaunchPlan,
    resolved: &ResolvedRobot,
    board: &BoardBackend,
    specs: &mut Vec<ParticipantSpec>,
    router_ownership: RouterOwnership,
) -> Result<()> {
    let namespace = plan
        .robots
        .first()
        .map(|robot| robot.namespace.as_str())
        .unwrap_or("site");
    let robot_id = plan
        .robots
        .first()
        .map(|robot| robot.id.as_str())
        .unwrap_or("site");

    for site in &plan.site {
        let should_launch =
            site.id != SITE_TOOL_ROUTER || router_ownership == RouterOwnership::Managed;
        let state = if should_launch {
            ParticipantState::Starting
        } else {
            ParticipantState::Ready
        };
        let mut status = ParticipantStatus::new(&site.id, ParticipantKind::SiteTool, state);
        if !should_launch {
            status.note = Some("external router reused".to_string());
        }
        board.upsert(status);
        if !should_launch {
            continue;
        }
        match locate_tool_binary(resolved, &site.id)? {
            Some(path) => specs.push(ParticipantSpec {
                id: site.id.clone(),
                kind: ParticipantKind::SiteTool,
                executable: path,
                args: Vec::new(),
                cwd: None,
                env: site_env(site, namespace, robot_id)?,
                shutdown_grace: Duration::from_secs(5),
                note: None,
            }),
            None => board.set_state(
                &site.id,
                ParticipantState::Failed,
                Some(native_pending_tool_note(&site.id)),
            ),
        }
    }
    Ok(())
}

pub(crate) fn prepare_robot_participants(
    plan: &LaunchPlan,
    resolved: &ResolvedRobot,
    _project_root: &Path,
    driver_policy: &DriverPolicy,
    board: &BoardBackend,
    specs: &mut Vec<ParticipantSpec>,
    ui: &crate::Ui,
) -> Result<()> {
    let official_by_name = resolved
        .platform_runtimes
        .iter()
        .map(|runtime| (runtime.name.as_str(), runtime))
        .collect::<BTreeMap<_, _>>();
    for robot in &plan.robots {
        for participant in &robot.participants {
            let id = participant.launch.participant_id.clone();
            let kind = participant_kind(&participant.execution);
            board.upsert(ParticipantStatus::new(
                &id,
                kind,
                ParticipantState::Starting,
            ));
            match &participant.execution {
                ParticipantExecution::OfficialArtifact { artifact_ref } => {
                    let runtime = official_by_name
                        .get(participant.artifact_id.as_str())
                        .copied();
                    match locate_official_binary(runtime, &participant.artifact_id, artifact_ref)? {
                        Some(path) => specs.push(ParticipantSpec {
                            id,
                            kind,
                            executable: path,
                            args: Vec::new(),
                            cwd: None,
                            env: encode_participant_env(&participant.launch)?.spawn_env(),
                            shutdown_grace: Duration::from_millis(
                                participant.launch.shutdown_grace_ms,
                            ),
                            note: None,
                        }),
                        None => board.set_state(
                            &participant.launch.participant_id,
                            ParticipantState::Failed,
                            Some(native_pending_official_note(
                                runtime,
                                &participant.artifact_id,
                            )),
                        ),
                    }
                }
                ParticipantExecution::UserService { crate_dir } => {
                    let binary = build_source_binary(crate_dir, &id, ui)?;
                    specs.push(ParticipantSpec {
                        id,
                        kind,
                        executable: binary,
                        args: Vec::new(),
                        cwd: Some(crate_dir.clone()),
                        env: encode_participant_env(&participant.launch)?.spawn_env(),
                        shutdown_grace: Duration::from_millis(participant.launch.shutdown_grace_ms),
                        note: None,
                    });
                }
                ParticipantExecution::SourceArtifact { crate_dir, .. } => {
                    let binary = build_source_binary(crate_dir, &id, ui)?;
                    specs.push(ParticipantSpec {
                        id,
                        kind,
                        executable: binary,
                        args: Vec::new(),
                        cwd: Some(crate_dir.clone()),
                        env: encode_participant_env(&participant.launch)?.spawn_env(),
                        shutdown_grace: Duration::from_millis(participant.launch.shutdown_grace_ms),
                        note: None,
                    });
                }
                ParticipantExecution::ComponentDriver { crate_dir } => {
                    match driver_policy.decision(&id) {
                        DriverDecision::Degraded(note) => {
                            board.set_state(&id, ParticipantState::Degraded, Some(note));
                            continue;
                        }
                        DriverDecision::Launch => {}
                    }
                    if cfg!(target_os = "macos") {
                        board.set_state(
                            &id,
                            ParticipantState::Failed,
                            Some("DriverUnsupported: component driver binaries are Linux-only on macOS (D21)".to_string()),
                        );
                        continue;
                    }
                    if let Some(note) = device_missing_note(resolved, &id) {
                        board.set_state(&id, ParticipantState::Failed, Some(note));
                        continue;
                    }
                    let binary = build_source_binary(crate_dir, &id, ui)?;
                    specs.push(ParticipantSpec {
                        id,
                        kind,
                        executable: binary,
                        args: Vec::new(),
                        cwd: Some(crate_dir.clone()),
                        env: encode_participant_env(&participant.launch)?.spawn_env(),
                        shutdown_grace: Duration::from_millis(participant.launch.shutdown_grace_ms),
                        note: None,
                    });
                }
            }
        }
    }
    Ok(())
}

fn participant_kind(execution: &ParticipantExecution) -> ParticipantKind {
    match execution {
        ParticipantExecution::OfficialArtifact { .. } => ParticipantKind::OfficialArtifact,
        ParticipantExecution::UserService { .. } => ParticipantKind::UserService,
        ParticipantExecution::SourceArtifact { kind, .. } if kind == "service" => {
            ParticipantKind::OfficialArtifact
        }
        ParticipantExecution::SourceArtifact { .. } => ParticipantKind::OfficialArtifact,
        ParticipantExecution::ComponentDriver { .. } => ParticipantKind::ComponentDriver,
    }
}

pub(crate) fn source_spec_from_launch_record(
    participant: &ParticipantLaunchRecord,
    ui: &crate::Ui,
) -> Result<Option<ParticipantSpec>> {
    let id = participant.launch.participant_id.clone();
    let kind = participant_kind(&participant.execution);
    let crate_dir = match &participant.execution {
        ParticipantExecution::UserService { crate_dir }
        | ParticipantExecution::SourceArtifact { crate_dir, .. }
        | ParticipantExecution::ComponentDriver { crate_dir } => crate_dir,
        ParticipantExecution::OfficialArtifact { .. } => return Ok(None),
    };
    let binary = build_source_binary(crate_dir, &id, ui)?;
    Ok(Some(ParticipantSpec {
        id,
        kind,
        executable: binary,
        args: Vec::new(),
        cwd: Some(crate_dir.clone()),
        env: encode_participant_env(&participant.launch)?.spawn_env(),
        shutdown_grace: Duration::from_millis(participant.launch.shutdown_grace_ms),
        note: None,
    }))
}

fn site_env(site: &SiteLaunch, namespace: &str, robot_id: &str) -> Result<Vec<(String, String)>> {
    let mut envs = vec![
        (env::PARTICIPANT_ID.to_string(), site.id.clone()),
        (env::NAMESPACE.to_string(), namespace.to_string()),
        (env::ROBOT_ID.to_string(), robot_id.to_string()),
        (
            env::CONFIG.to_string(),
            serde_json::to_string(&site.phoxal_config)
                .with_context(|| format!("failed to encode PHOXAL_CONFIG for {}", site.id))?,
        ),
        (env::CLOCK.to_string(), "real".to_string()),
    ];
    if site.id == SITE_TOOL_JOYPAD {
        envs.push((env::CONNECT.to_string(), default_connect_endpoint()));
    }
    Ok(envs)
}

fn locate_tool_binary(resolved: &ResolvedRobot, name: &str) -> Result<Option<PathBuf>> {
    let tool = resolved
        .tools
        .iter()
        .find(|tool| tool.name == name)
        .ok_or_else(|| anyhow!("resolved graph is missing site tool {name}"))?;
    if let Some(path) = &tool.path_override {
        return Ok(Some(build_source_binary(path, name, &crate::Ui)?));
    }
    if let Some(path) = env_path_override("PHOXAL_ARTIFACT", name) {
        return Ok(Some(path));
    }
    if let Some(path) = env_path_override("PHOXAL_TOOL", name) {
        return Ok(Some(path));
    }
    if let Ok(dir) = std::env::var("PHOXAL_ARTIFACT_DIR") {
        let path = PathBuf::from(dir).join(&tool.binary_name);
        if path.is_file() {
            return Ok(Some(path));
        }
    }
    if let Ok(dir) = std::env::var("PHOXAL_TOOL_DIR") {
        for name in [&tool.name, &tool.binary_name] {
            let path = PathBuf::from(&dir).join(name);
            if path.is_file() {
                return Ok(Some(path));
            }
        }
    }
    let Some(descriptor) = crate::native_artifacts::NativeArtifactDescriptor::from_tool(tool)?
    else {
        return Ok(None);
    };
    let cache = crate::native_artifacts::artifact_binary_path(&descriptor)?;
    Ok(cache.is_file().then_some(cache))
}

fn locate_official_binary(
    runtime: Option<&ResolvedPlatformRuntime>,
    participant_id: &str,
    artifact_ref: &str,
) -> Result<Option<PathBuf>> {
    if let Some(path) = env_path_override("PHOXAL_ARTIFACT", participant_id) {
        return Ok(Some(path));
    }
    let binary_name = runtime
        .map(|runtime| format!("phoxal-{}-{}", runtime.kind, runtime.name))
        .unwrap_or_else(|| participant_id.to_string());
    if let Ok(dir) = std::env::var("PHOXAL_ARTIFACT_DIR") {
        let path = PathBuf::from(dir).join(&binary_name);
        if path.is_file() {
            return Ok(Some(path));
        }
    }
    if let Some(runtime) = runtime
        && let Some(descriptor) =
            crate::native_artifacts::NativeArtifactDescriptor::from_runtime(runtime)?
    {
        let cache = crate::native_artifacts::artifact_binary_path(&descriptor)?;
        return Ok(cache.is_file().then_some(cache));
    }
    let cache = crate::host_paths::cache_dir()?
        .join("artifacts")
        .join(participant_id)
        .join(sanitize_path_segment(artifact_ref))
        .join(binary_name);
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

fn sanitize_path_segment(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_') {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

fn native_pending_tool_note(name: &str) -> String {
    format!(
        "NativePending: {name} binary is not in the artifact cache; set PHOXAL_ARTIFACT_{}_PATH, PHOXAL_ARTIFACT_DIR, PHOXAL_TOOL_{}_PATH, or PHOXAL_TOOL_DIR",
        env_key(name),
        env_key(name)
    )
}

fn native_pending_official_note(
    runtime: Option<&ResolvedPlatformRuntime>,
    participant_id: &str,
) -> String {
    let status = runtime
        .and_then(|runtime| runtime.target_status)
        .map(|status| status.to_string())
        .unwrap_or_else(|| "missing".to_string());
    let target = host_target_triple();
    format!(
        "NativePending: official artifact {participant_id} is {status} for {target} or not cached; run `phoxal-cli pull`, set PHOXAL_ARTIFACT_{}_PATH, or set PHOXAL_ARTIFACT_DIR",
        env_key(participant_id)
    )
}

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
    ui.info(format!(
        "building user participant {preferred_name} with cargo build --bin {binary_name}"
    ));
    let mut command = Command::new("cargo");
    command
        .arg("build")
        .arg("--bin")
        .arg(&binary_name)
        .current_dir(&crate_dir);
    let status = ui.command_status(&mut command).with_context(|| {
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
    Ok(cargo_target_dir(&crate_dir)?
        .join("debug")
        .join(binary_name_with_suffix(&binary_name)))
}

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

fn binary_name_with_suffix(binary_name: &str) -> String {
    if cfg!(windows) {
        format!("{binary_name}.exe")
    } else {
        binary_name.to_string()
    }
}

fn device_missing_note(resolved: &ResolvedRobot, participant_id: &str) -> Option<String> {
    let component = resolved.robot.components.instances.get(participant_id)?;
    let driver = component.driver.as_ref()?;
    let missing = missing_device_path(&driver.connection)?;
    Some(format!(
        "DeviceMissing: {missing} for driver {participant_id}"
    ))
}

fn missing_device_path(connection: &ConnectionConfig) -> Option<String> {
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

fn usb_missing(vendor_id: Option<u16>, product_id: Option<u16>) -> Option<String> {
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
mod tests {
    use super::*;
    use crate::launch_plan::ParticipantLaunchRecord;
    use phoxal::participant::launch::{
        BusProfile, ClockMode, DEFAULT_SHUTDOWN_GRACE_MS, ParticipantLaunch,
    };

    fn participant(id: &str, execution: ParticipantExecution) -> ParticipantLaunchRecord {
        ParticipantLaunchRecord {
            artifact_id: id.to_string(),
            execution,
            launch: ParticipantLaunch {
                participant_id: id.to_string(),
                namespace: "dev".to_string(),
                robot_id: "robot".to_string(),
                bus: BusProfile {
                    connect_endpoints: vec![default_connect_endpoint()],
                },
                clock: ClockMode::Real,
                config: None,
                robot_root: Some(PathBuf::from("/tmp/robot")),
                component_instance: None,
                shutdown_grace_ms: DEFAULT_SHUTDOWN_GRACE_MS,
            },
        }
    }

    fn plan_with_drivers(ids: &[&str]) -> LaunchPlan {
        LaunchPlan {
            mode: LaunchMode::Run,
            site: Vec::new(),
            robots: vec![crate::launch_plan::RobotLaunch {
                id: "robot".to_string(),
                namespace: "dev".to_string(),
                participants: ids
                    .iter()
                    .map(|id| {
                        participant(
                            id,
                            ParticipantExecution::ComponentDriver {
                                crate_dir: PathBuf::from("/tmp/driver"),
                            },
                        )
                    })
                    .collect(),
                substitutions: Vec::new(),
            }],
        }
    }

    #[test]
    fn driver_subset_is_strict() -> Result<()> {
        let plan = plan_with_drivers(&["imu", "left_drive"]);
        let policy = DriverPolicy::from_options(
            &RunOptions {
                pull: false,
                drivers: DriversMode::On,
                drivers_subset: vec!["imu".to_string()],
                catalog_source: None,
                overlays: Vec::new(),
                watch: false,
                message_format: MessageFormat::Human,
            },
            &plan,
        )?;
        assert_eq!(policy.decision("imu"), DriverDecision::Launch);
        assert_eq!(
            policy.decision("left_drive"),
            DriverDecision::Degraded("not selected by --driver".to_string())
        );

        let err = DriverPolicy::from_options(
            &RunOptions {
                pull: false,
                drivers: DriversMode::On,
                drivers_subset: vec!["missing".to_string()],
                catalog_source: None,
                overlays: Vec::new(),
                watch: false,
                message_format: MessageFormat::Human,
            },
            &plan,
        )
        .expect_err("unknown drivers must fail");
        assert!(err.to_string().contains("unknown driver id"));
        Ok(())
    }

    #[test]
    fn drivers_off_degrades_every_driver() -> Result<()> {
        let plan = plan_with_drivers(&["imu"]);
        let policy = DriverPolicy::from_options(
            &RunOptions {
                pull: false,
                drivers: DriversMode::Off,
                drivers_subset: Vec::new(),
                catalog_source: None,
                overlays: Vec::new(),
                watch: false,
                message_format: MessageFormat::Human,
            },
            &plan,
        )?;
        assert_eq!(
            policy.decision("imu"),
            DriverDecision::Degraded("drivers off".to_string())
        );
        Ok(())
    }

    #[test]
    fn path_override_env_key_is_stable() {
        assert_eq!(env_key("tool-router"), "TOOL_ROUTER");
        assert_eq!(env_key("left_drive"), "LEFT_DRIVE");
    }

    #[test]
    fn serial_device_missing_is_loud() {
        let missing = missing_device_path(&ConnectionConfig::Serial {
            port: "/definitely/not/a/phoxal/device".to_string(),
            baud: 115200,
        });
        assert_eq!(missing.as_deref(), Some("/definitely/not/a/phoxal/device"));
    }
}
