use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use clap::{Args, ValueEnum};
use phoxal::model::robot::v0::ConnectionConfig;
use phoxal::participant::launch::env;
use serde::Serialize;
use serde_json::Value;
use tokio::sync::mpsc;

use crate::AppContext;
use crate::commands::MessageFormat;
use crate::commands::check::{
    CheckGraphContext, build_emit_apis_from_source, check_artifact_refs_from_resolved,
    extract_emit_apis_from_staged_runtime, extract_emit_apis_from_staged_tool,
    fetch_emit_apis_from_tool, run_check_with_context, source_participants_from_resolved,
    tool_participants_from_resolved,
};
use crate::component_driver::component_driver_crate_dir;
use crate::launch_env::encode_participant_env;
use crate::launch_plan::{
    CheckedRobotLaunchInput, LaunchMode, LaunchOwnership, LaunchPlan, ParticipantExecution,
    ParticipantLaunchRecord, PlanContext, SITE_TOOL_ROUTER, SiteLaunch, build_launch_plan,
};
use crate::resolver::{
    ResolveOptions, ResolvedPlatformRuntime, ResolvedRobot, discover_robot_yaml,
    host_target_triple, load_robot_with_extras, resolve,
};
use crate::supervisor::{
    BoardBackend, ParticipantKind, ParticipantSpec, ParticipantState, ParticipantStatus,
    RouterOwnership, SupervisionStage, SupervisorLock, SupervisorOptions, default_connect_endpoint,
    local_router_reachable, router_ownership, start_bus_log_subscriber,
    start_presence_heartbeat_subscriber, supervise_until_shutdown, supervisor_actions_path,
    supervisor_state_path,
};
use crate::utils::cargo_binary_name;

/// How long a `run` staged-startup stage may wait for its members to be
/// OBSERVED ready before the whole run fails naming the stalled stage - see
/// `stages_for_run` and `supervisor::SupervisionStage`. Every `run`
/// participant is CLI-managed and expected to clear its own `#[setup]`
/// quickly on a loaded host; generous enough to absorb ordinary scheduling
/// jitter without masking a genuinely hung participant.
const RUN_STAGE_READY_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Args)]
pub struct Run {
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
    pub drivers: DriversMode,
    pub drivers_subset: Vec<String>,
    pub catalog_source: Option<String>,
    pub overlays: Vec<String>,
    pub watch: bool,
    pub message_format: MessageFormat,
    /// The session's output mode, threaded into a catalog fetch's spinner
    /// (`watch::recheck_run_target` runs `--watch` rechecks with no
    /// `AppContext` in scope) - no process-global mode cell.
    pub output_mode: crate::output_mode::OutputMode,
}

#[derive(Debug)]
struct PreparedRun {
    ctx: PlanContext,
    plan: LaunchPlan,
    board: BoardBackend,
    specs: Vec<ParticipantSpec>,
    robot_log_targets: Vec<(String, String)>,
    router_ownership: RouterOwnership,
    /// Finding A5: this session's launch-time participant metadata, resolved
    /// once here from `plan` and the contract-check `outcome` - see
    /// `crate::stores::runtime_store::RuntimeStore`'s own docs.
    runtime_store: crate::stores::runtime_store::RuntimeStore,
}

impl Run {
    pub async fn run(&self, app: &AppContext) -> Result<()> {
        let options = RunOptions {
            drivers: self.drivers,
            drivers_subset: self.drivers_subset.clone(),
            catalog_source: app.catalog_source.clone(),
            overlays: self.env.clone(),
            watch: self.watch,
            message_format: self.message_format,
            output_mode: app.output.mode,
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

        // One interactive surface for the whole session (Product decision
        // 1): the controller starts its renderer (a TUI's alternate screen,
        // or the append-only line renderer) right now, before preparation
        // even begins - see `SessionController::new`'s docs.
        let identity = crate::identity::IdentitySummary::discover(app.project.root());
        let mut controller =
            crate::session::controller::SessionController::new(app.output, "run", identity)?;
        let events = controller.events();

        let prepared = controller
            .drive_prepare_phase(move || prepare_run(&project_root, options, &ui))
            .await?;

        app.ui.info(format!(
            "launch plan resolved: {} robot(s), {} site tool(s)",
            prepared.plan.robots.len(),
            prepared.plan.site.len()
        ));
        match prepared.router_ownership {
            RouterOwnership::External => app.ui.info("reusing reachable external tool-router"),
            RouterOwnership::Managed => app.ui.info("tool-router will be managed by this session"),
        }
        report_launch_commands(&prepared.plan, &prepared.specs, message_format)?;

        // Finding B6: every feed task participates in teardown instead of
        // being leaked under a `_`-prefixed binding for the rest of the
        // process's life - collected here and aborted once supervision ends
        // (see below), rather than detached.
        let mut feed_tasks = prepared
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
        // OBSERVED readiness: drive board state from each participant's own
        // presence/heartbeat, mirroring the log subscriber above - see
        // `supervisor::start_presence_heartbeat_subscriber`.
        feed_tasks.extend(
            prepared
                .robot_log_targets
                .iter()
                .map(|(namespace, robot_id)| {
                    start_presence_heartbeat_subscriber(
                        namespace.clone(),
                        robot_id.clone(),
                        default_connect_endpoint(),
                        prepared.board.clone(),
                    )
                }),
        );

        // The restart/hot-reload action channel always exists now (not just
        // under `--watch`), so the TUI's `r restart` reaches the supervisor
        // through `SessionController::set_restart_channel` even when `--watch`
        // is off.
        let (action_tx, action_rx) = mpsc::channel(16);
        controller.set_restart_channel(action_tx.clone());
        let watch_handle = if watch_enabled {
            let live_ids = prepared
                .specs
                .iter()
                .map(|spec| spec.id.clone())
                .collect::<BTreeSet<_>>();
            Some(crate::watch::spawn_run_watch(
                crate::watch::RunWatchConfig {
                    ctx: prepared.ctx.clone(),
                    options: watch_options,
                    live_ids,
                    board: prepared.board.clone(),
                    action_tx,
                },
            ))
        } else {
            None
        };

        let stages = stages_for_run(prepared.specs, app.output);
        // `run` has no simulation clock (Product decision 5 is
        // `simulation run`-specific), so its session never visits
        // `Waiting`/`Paused`. Fixes finding B3: it used to claim `Running`
        // immediately, before the supervisor task even existed - now it only
        // announces `Starting` here; `Running` is instead emitted by
        // `supervise_until_shutdown` itself (`emits_running_on_startup_complete`
        // below), once every staged-startup stage has ACTUALLY spawned and
        // been observed ready, via `SessionEvent::StagedStartupComplete`. Per-
        // stage progress is already visible via the `PhaseStarted`/
        // `PhaseFinished` events `supervise_until_shutdown` itself emits.
        let starting = crate::session::state::SessionState::Preparing
            .start()
            .expect("the controller begins every session in Preparing");
        // Lifecycle events are awaited, not `try_send` (finding B5): a
        // session-state transition must never be silently dropped under
        // channel pressure.
        let _ = events
            .send(crate::session::event::SessionEvent::SessionChanged { state: starting })
            .await;
        // Live telemetry (CLI-UX Phase 3): only worth subscribing when a real
        // TUI is up to read it - `--message-format json`/non-interactive
        // sessions never touch `telemetry`, so skip the extra bus
        // connections entirely rather than feed a renderer that can't show
        // them. `run` has no simulation clock (`telemetry::TelemetryBackend`
        // is never given one here - see `commands::simulate` for the
        // sim-clock feed), so the TUI's clock slot stays empty in this mode
        // by design (`tui::render::simulation_clock_slot`).
        let telemetry = crate::telemetry::TelemetryBackend::new();
        if controller.renders_tui() {
            feed_tasks.extend(start_telemetry_feeds(
                &prepared.robot_log_targets,
                &telemetry,
            ));
        }

        let board = prepared.board.clone();
        let supervise = tokio::spawn(supervise_until_shutdown(
            stages,
            prepared.board,
            SupervisorOptions {
                state_file: Some(state_file),
                action_file: Some(action_file),
                action_rx: Some(action_rx),
                token: controller.token(),
                events: Some(events),
                emits_running_on_startup_complete: true,
                ..SupervisorOptions::default()
            },
        ));

        let outcome = controller
            .drive_supervision(board, telemetry, prepared.runtime_store, supervise)
            .await;
        if let Some(handle) = watch_handle {
            handle.abort();
        }
        for feed in feed_tasks {
            feed.abort();
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

/// Partition an already-built `run` spec list into the staged startup order
/// (Part 2): router < other tools (`tool-joypad`, `tool-telemetry`) < drivers
/// < services. Each stage's members all spawn together,
/// then the whole stage must be OBSERVED ready (transport probe for the
/// router - see its `bus_participant: false` - heartbeat for everything
/// else) before the next stage spawns; see `supervisor::SupervisionStage`
/// and `supervisor::await_participants_ready`.
fn stages_for_run(
    specs: Vec<ParticipantSpec>,
    output: crate::session::output::OutputContext,
) -> Vec<SupervisionStage> {
    let mut router = Vec::new();
    let mut tools = Vec::new();
    let mut drivers = Vec::new();
    let mut services = Vec::new();
    for spec in specs {
        if spec.id == SITE_TOOL_ROUTER {
            router.push(spec);
        } else {
            match spec.kind {
                ParticipantKind::Tool => tools.push(spec),
                ParticipantKind::Driver => drivers.push(spec),
                ParticipantKind::Service | ParticipantKind::Simulator => services.push(spec),
            }
        }
    }
    // Product decision 6: no unconditional 60s teardown for an interactive
    // session - see `OutputContext::wait_budget`.
    let timeout = output.wait_budget(RUN_STAGE_READY_TIMEOUT);
    vec![
        SupervisionStage::new("starting router", router, timeout),
        SupervisionStage::new("starting tools", tools, timeout),
        SupervisionStage::new("starting drivers", drivers, timeout),
        SupervisionStage::new("starting services", services, timeout),
    ]
}

/// Start the host/process/router-metrics/joypad-devices telemetry feeds
/// (CLI-UX Phase 3/4) against the first robot's bus namespace - the site
/// tools they subscribe to (`tool-telemetry`, `tool-router`, `tool-joypad`)
/// are session-scoped, not per-robot, exactly like `prepare_site_tools`'s own
/// namespace/robot_id choice. Harmless to call even when one or more of
/// those tools never resolved (`launch_plan::build_site_launches`'s graceful
/// telemetry-absence path, or `--drivers off`-style opt-outs): a subscriber
/// for a topic nobody publishes to simply never receives a sample, which is
/// exactly the graceful-absence rendering the TUI already handles (`cpu
/// n/a`). Shared by both `run` and `commands::simulate` (`simulate` wires in
/// the sim-clock feed separately - see `TelemetryBackend::set_clock_feed`).
pub(crate) fn start_telemetry_feeds(
    robot_log_targets: &[(String, String)],
    telemetry: &crate::telemetry::TelemetryBackend,
) -> Vec<tokio::task::JoinHandle<()>> {
    let Some((namespace, robot_id)) = robot_log_targets.first() else {
        return Vec::new();
    };
    vec![
        crate::telemetry::start_host_feed(
            namespace.clone(),
            robot_id.clone(),
            default_connect_endpoint(),
            telemetry.clone(),
        ),
        crate::telemetry::start_process_feed(
            namespace.clone(),
            robot_id.clone(),
            default_connect_endpoint(),
            telemetry.clone(),
        ),
        crate::telemetry::start_router_metrics_feed(
            namespace.clone(),
            robot_id.clone(),
            default_connect_endpoint(),
            telemetry.clone(),
        ),
        crate::telemetry::start_joypad_devices_feed(
            namespace.clone(),
            robot_id.clone(),
            default_connect_endpoint(),
            telemetry.clone(),
        ),
    ]
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
        loaded.robot.artifacts.channel,
        &loaded.extras,
        ui.mode(),
    )?;
    let resolved = resolve(
        &loaded.robot,
        project_root,
        catalog.as_ref(),
        ResolveOptions {
            emit_update_notice: true,
            resolve_source_commits: true,
            resolve_component_asset_commits: false,
            ..ResolveOptions::default()
        },
    )?;
    let descriptors = crate::native_artifacts::descriptors_for(&resolved, false, true)?;
    crate::native_artifacts::prepare_descriptors_with_preflight(&descriptors, Some(ui))?;

    // Stage every resolved component's asset bundle into the robot root
    // (`project_root` for `run`) so `PHOXAL_ROBOT_ROOT`-relative asset
    // resolution finds the same `components/<id>/` shape deploy stages under
    // `/opt/phoxal/` (docs #21). A no-op for a `Path`-pinned component whose
    // files already live there.
    if let Err(error) = crate::native_artifacts::stage_component_bundles_into_robot_root(
        project_root,
        project_root,
        &resolved,
    ) {
        ui.warn(format!(
            "component asset staging into the robot root failed: {error:#}"
        ));
    }

    let source_participants =
        source_participants_from_resolved(project_root, &resolved, component_driver_crate_dir)?;
    let platform_refs = check_artifact_refs_from_resolved(&resolved);
    let tool_participants = tool_participants_from_resolved(&resolved)?;
    let mut official_by_ref = resolved
        .platform_runtimes
        .iter()
        .map(|runtime| (runtime.artifact_ref().to_string(), runtime))
        .collect::<BTreeMap<_, _>>();
    official_by_ref.extend(crate::commands::check::component_driver_runtimes_by_ref(
        &resolved,
    ));
    let tools_by_ref = resolved
        .tools
        .iter()
        .map(|tool| (tool.asset.clone(), tool))
        .collect::<BTreeMap<_, _>>();
    let outcome = run_check_with_context(
        &platform_refs,
        &tool_participants,
        &source_participants,
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
    if !outcome.is_ok() {
        crate::commands::check::ensure_check_outcome_ok(&resolved.channel.to_string(), &outcome)?;
    }
    let plan = build_launch_plan(
        LaunchMode::Run,
        &[CheckedRobotLaunchInput {
            project_root,
            resolved: &resolved,
            manifest_extras: &loaded.extras,
            checked_participants: &outcome.checked_participants,
            substitutions: &[],
            source_participants: &source_participants,
        }],
    )?;
    let driver_policy = DriverPolicy::from_options(&options, &plan)?;
    let mut coherence_plan = plan.clone();
    for robot in &mut coherence_plan.robots {
        robot
            .participants
            .retain(|participant| driver_policy.launches(participant));
    }
    let coherence_graph = crate::commands::check::robot_contract_surfaces(
        &resolved.robot.robot.id,
        &outcome.contract_surfaces,
    );
    let coherence =
        crate::commands::check::coherence_for_launch_plan(&coherence_plan, &[coherence_graph])?;
    crate::commands::check::enforce_coherence(
        crate::commands::check::CoherenceVerb::Run,
        &coherence,
        options.message_format,
    )?;
    // Finding A5: resolved once here, from the same `plan`/`outcome` this
    // function already built - see `RuntimeStore::from_launch_plan`'s docs.
    let runtime_store = crate::stores::runtime_store::RuntimeStore::from_launch_plan(
        &plan,
        &outcome.contract_surfaces,
    );
    let board = BoardBackend::new();
    let router_ownership = router_ownership(local_router_reachable(&default_connect_endpoint()));
    let mut specs = Vec::new();

    prepare_site_tools(&plan, &resolved, &board, &mut specs, router_ownership, ui)?;
    prepare_robot_participants(
        &plan,
        &resolved,
        project_root,
        &driver_policy,
        &board,
        &mut specs,
        ui,
    )?;

    let robot_log_targets = plan
        .robots
        .iter()
        .map(|robot| (robot.namespace.clone(), robot.id.clone()))
        .collect();
    let project_root = project_root.to_path_buf();
    let ctx = PlanContext {
        robot_path,
        project_root,
        resolved,
        source_participants,
    };

    Ok(PreparedRun {
        ctx,
        robot_log_targets,
        plan,
        board,
        specs,
        router_ownership,
        runtime_store,
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

/// The pre-staged-startup (Phase 0) launch-report `kind` string, preserved
/// byte-for-byte for `--message-format json` backward compatibility even
/// though the board's own `ParticipantKind` is now the finer-grained shared
/// `Tool`/`Service`/`Driver`/`Simulator` split plus a `local` bit (Part 1) -
/// see `participant_kind`'s module docs. A site launch (the router, the
/// joypad, the Webots app in `simulate`) has no `ParticipantExecution` of
/// its own and is always `"site-tool"`; everything else follows the
/// pre-consolidation mapping this report has always used.
fn legacy_launch_kind_label(execution: Option<&ParticipantExecution>) -> &'static str {
    match execution {
        None => "site-tool",
        Some(
            ParticipantExecution::OfficialArtifact { .. }
            | ParticipantExecution::SourceArtifact { .. },
        ) => "official",
        Some(ParticipantExecution::UserService { .. }) => "user-service",
        Some(ParticipantExecution::ComponentDriver { .. }) => "driver",
    }
}

pub(crate) fn report_launch_commands(
    plan: &LaunchPlan,
    specs: &[ParticipantSpec],
    message_format: MessageFormat,
) -> Result<()> {
    if message_format != MessageFormat::Json {
        return Ok(());
    }
    let executions_by_id = plan
        .robots
        .iter()
        .flat_map(|robot| &robot.participants)
        .map(|participant| {
            (
                participant.launch.participant_id.as_str(),
                &participant.execution,
            )
        })
        .collect::<BTreeMap<_, _>>();
    let output = LaunchCommandReport {
        participants: specs
            .iter()
            .map(|spec| {
                let launch = spec.launch_command();
                LaunchCommandEntry {
                    id: spec.id.clone(),
                    kind: legacy_launch_kind_label(executions_by_id.get(spec.id.as_str()).copied()),
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

    pub(crate) fn launches(&self, participant: &ParticipantLaunchRecord) -> bool {
        !matches!(
            participant.execution,
            ParticipantExecution::ComponentDriver { .. }
        ) || self.decision(&participant.launch.participant_id) == DriverDecision::Launch
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
    ui: &crate::Ui,
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
        // A site tool (the router transport, or the joypad peripheral) is not
        // a contract-graph participant - it is not gated by OBSERVED
        // readiness. The router's own framework runner bus is in-process (no
        // `PHOXAL_CONNECT`), so its heartbeat is structurally unobservable by
        // the CLI's presence subscriber; its liveness is transitively proven
        // instead by every downstream participant that talks through it
        // reaching `Ready` on the barrier. So readiness here is process /
        // transport liveness, not a heartbeat: `Ready` immediately once
        // spawned (managed) or immediately here (external, since
        // `RouterOwnership::External` already means `local_router_reachable`
        // proved the transport is up).
        let initial_state = if should_launch {
            ParticipantState::Starting
        } else {
            ParticipantState::Ready
        };
        let mut status = ParticipantStatus::new(&site.id, ParticipantKind::Tool, initial_state)
            .with_local(site_tool_is_local(resolved, &site.id));
        if !should_launch {
            status.note = Some("external router reused".to_string());
        }
        board.upsert(status);
        if !should_launch {
            continue;
        }
        match locate_tool_binary(resolved, &site.id, ui)? {
            Some(path) => specs.push(ParticipantSpec {
                id: site.id.clone(),
                kind: ParticipantKind::Tool,
                local: site_tool_is_local(resolved, &site.id),
                executable: path,
                args: Vec::new(),
                cwd: None,
                env: site_env(site, namespace, robot_id)?,
                shutdown_grace: Duration::from_secs(5),
                process_group: false,
                note: None,
                // The router's own framework runner bus is in-process (no
                // `PHOXAL_CONNECT`), so its heartbeat is structurally
                // unobservable by the CLI's presence subscriber - it keeps
                // the old spawn-is-ready behavior, gated in the staged
                // startup by the transport probe (`router_ownership`/
                // `local_router_reachable`) instead of a heartbeat. Every
                // OTHER site tool (`tool-joypad`, `tool-telemetry`) is a real
                // bus participant and can be gated like any other stage
                // member. `tool-telemetry` simply never appears in
                // `plan.site` at all when the catalog snapshot in use
                // predates it (`launch_plan::build_site_launches`), so this
                // loop never has to special-case its absence.
                bus_participant: site.id != SITE_TOOL_ROUTER,
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
            let (kind, local) = participant_kind(&participant.execution);
            if participant.launch_ownership == LaunchOwnership::SimulationManaged {
                // Webots (via the supervisor) owns this participant's
                // lifecycle - the CLI never spawns or restarts it, and has no
                // process to poll for readiness. It still satisfies the graph
                // proof and appears on the board, starting `Starting`, not
                // `Ready`: OBSERVED readiness comes from its own bus
                // heartbeats (D23), same as any participant, driven by
                // `BoardBackend::record_heartbeat` once the presence
                // heartbeat subscriber is running. A controller/supervisor
                // Webots never actually launches (or that silently crashes
                // before its own `#[setup]` completes) therefore never
                // reaches `Ready` here, and the simulate readiness barrier
                // (`await_readiness_barrier`) - or, failing that, the
                // heartbeat staleness sweep - is what turns that into a
                // detected failure instead of a permanently green board.
                // `commands::simulate` renders its controllerArgs into the
                // staged world instead of a `ParticipantSpec` (Part 5).
                let mut status =
                    ParticipantStatus::new(&id, kind, ParticipantState::Starting).with_local(local);
                status.note = Some(
                    "SimulationManaged: launched by Webots via the supervisor, not the CLI supervisor"
                        .to_string(),
                );
                board.upsert(status);
                continue;
            }
            board.upsert(
                ParticipantStatus::new(&id, kind, ParticipantState::Starting).with_local(local),
            );
            match &participant.execution {
                ParticipantExecution::OfficialArtifact { .. } => {
                    let runtime = official_by_name
                        .get(participant.artifact_id.as_str())
                        .copied();
                    match locate_official_binary(runtime, &participant.artifact_id)? {
                        Some(path) => specs.push(ParticipantSpec {
                            id,
                            kind,
                            local,
                            executable: path,
                            args: Vec::new(),
                            cwd: None,
                            env: encode_participant_env(&participant.launch)?.spawn_env(),
                            shutdown_grace: Duration::from_millis(
                                participant.launch.shutdown_grace_ms,
                            ),
                            process_group: false,
                            note: None,
                            bus_participant: true,
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
                        local,
                        executable: binary,
                        args: Vec::new(),
                        cwd: Some(crate_dir.clone()),
                        env: encode_participant_env(&participant.launch)?.spawn_env(),
                        shutdown_grace: Duration::from_millis(participant.launch.shutdown_grace_ms),
                        process_group: false,
                        note: None,
                        bus_participant: true,
                    });
                }
                ParticipantExecution::SourceArtifact { crate_dir, .. } => {
                    let binary = build_source_binary(crate_dir, &id, ui)?;
                    specs.push(ParticipantSpec {
                        id,
                        kind,
                        local,
                        executable: binary,
                        args: Vec::new(),
                        cwd: Some(crate_dir.clone()),
                        env: encode_participant_env(&participant.launch)?.spawn_env(),
                        shutdown_grace: Duration::from_millis(participant.launch.shutdown_grace_ms),
                        process_group: false,
                        note: None,
                        bus_participant: true,
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
                        local,
                        executable: binary,
                        args: Vec::new(),
                        cwd: Some(crate_dir.clone()),
                        env: encode_participant_env(&participant.launch)?.spawn_env(),
                        shutdown_grace: Duration::from_millis(participant.launch.shutdown_grace_ms),
                        process_group: false,
                        note: None,
                        bus_participant: true,
                    });
                }
            }
        }
    }
    Ok(())
}

/// The board `ParticipantKind` plus whether the participant runs from a
/// locally resolved directory, for a checked participant's `execution`.
/// `SourceArtifact`'s own `kind: String` (`"tool"`/`"simulator"`/`"service"`,
/// set by `launch_plan::participant_execution` from
/// `check::SourceParticipantKind::shared_kind`) recovers the real role for a
/// locally source-overridden official artifact - a Run-mode launch plan only
/// ever contains Service and Driver participants (`Tool` and `Simulator`
/// checked participants are excluded upstream by
/// `launch_plan::is_robot_launch_participant`), so `"service"` is the only
/// value seen here in practice, but Sim-mode plans reuse this same helper via
/// `source_spec_from_launch_record` (through `watch`), where a
/// source-overridden simulator is possible.
fn participant_kind(execution: &ParticipantExecution) -> (ParticipantKind, bool) {
    match execution {
        ParticipantExecution::OfficialArtifact { .. } => (ParticipantKind::Service, false),
        ParticipantExecution::UserService { .. } => (ParticipantKind::Service, true),
        ParticipantExecution::SourceArtifact { kind, .. } => {
            let kind = match kind.as_str() {
                "tool" => ParticipantKind::Tool,
                "simulator" => ParticipantKind::Simulator,
                _ => ParticipantKind::Service,
            };
            (kind, true)
        }
        ParticipantExecution::ComponentDriver { .. } => (ParticipantKind::Driver, true),
    }
}

pub(crate) fn source_spec_from_launch_record(
    participant: &ParticipantLaunchRecord,
    ui: &crate::Ui,
) -> Result<Option<ParticipantSpec>> {
    let id = participant.launch.participant_id.clone();
    let (kind, local) = participant_kind(&participant.execution);
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
        local,
        executable: binary,
        args: Vec::new(),
        cwd: Some(crate_dir.clone()),
        env: encode_participant_env(&participant.launch)?.spawn_env(),
        shutdown_grace: Duration::from_millis(participant.launch.shutdown_grace_ms),
        process_group: false,
        note: None,
        bus_participant: true,
    }))
}

fn site_env(site: &SiteLaunch, namespace: &str, robot_id: &str) -> Result<Vec<(String, String)>> {
    let mut envs = vec![
        (env::PARTICIPANT_ID.to_string(), site.id.clone()),
        (env::NAMESPACE.to_string(), namespace.to_string()),
        (env::ROBOT_ID.to_string(), robot_id.to_string()),
        (env::CLOCK.to_string(), "real".to_string()),
    ];
    // A configless tool (`phoxal_config == Value::Null`, e.g. joypad/telemetry)
    // must run with `PHOXAL_CONFIG` ABSENT: a unit config (`type Config = ()`)
    // fails to deserialize `{}` ("invalid type: map, expected unit"), and an
    // absent var uses the runner's null/unit fallback. Only a tool that carries
    // real config (the router) gets `PHOXAL_CONFIG`.
    if !site.phoxal_config.is_null() {
        envs.push((
            env::CONFIG.to_string(),
            serde_json::to_string(&site.phoxal_config)
                .with_context(|| format!("failed to encode PHOXAL_CONFIG for {}", site.id))?,
        ));
    }
    // Every site tool OTHER than the router is a real bus client (joypad and
    // telemetry are observable bus participants), so it needs the connect
    // endpoint to reach the router's bus and publish its heartbeat/telemetry.
    // The router itself is the transport and takes no `PHOXAL_CONNECT`.
    if site.id != SITE_TOOL_ROUTER {
        envs.push((env::CONNECT.to_string(), default_connect_endpoint()));
    }
    Ok(envs)
}

/// Whether a site tool (`tool-router`/`tool-joypad`) is resolved from a local
/// path-pin override rather than a fetched catalog artifact. Best-effort:
/// `false` if the tool is missing from `resolved.tools` (surfaced properly by
/// `locate_tool_binary`'s own lookup instead).
fn site_tool_is_local(resolved: &ResolvedRobot, name: &str) -> bool {
    resolved
        .tools
        .iter()
        .find(|tool| tool.name == name)
        .is_some_and(|tool| tool.path_override.is_some())
}

fn locate_tool_binary(
    resolved: &ResolvedRobot,
    name: &str,
    ui: &crate::Ui,
) -> Result<Option<PathBuf>> {
    let tool = resolved
        .tools
        .iter()
        .find(|tool| tool.name == name)
        .ok_or_else(|| anyhow!("resolved graph is missing site tool {name}"))?;
    if let Some(path) = &tool.path_override {
        return Ok(Some(build_source_binary(path, name, ui)?));
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
) -> Result<Option<PathBuf>> {
    if let Some(path) = env_path_override("PHOXAL_ARTIFACT", participant_id) {
        return Ok(Some(path));
    }
    let binary_name = runtime
        .map(|runtime| crate::resolver::official_binary_name(runtime.kind, &runtime.name))
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
        let binary = crate::native_artifacts::artifact_binary_path(&descriptor)?;
        return Ok(binary.is_file().then_some(binary));
    }
    // No env override, and no resolved runtime to derive a native-artifact
    // descriptor from (a path-overridden or otherwise non-catalog runtime) -
    // the project-local store has no other identity from which to find this
    // participant's binary.
    Ok(None)
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
    let status = match runtime {
        Some(runtime) if runtime.published => "released",
        _ => "missing",
    };
    let target = host_target_triple();
    format!(
        "NativePending: official artifact {participant_id} is {status} for {target} or not vendored; run `phoxal update`, set PHOXAL_ARTIFACT_{}_PATH, or set PHOXAL_ARTIFACT_DIR",
        env_key(participant_id)
    )
}

/// Build one user participant's crate. Fixes findings A2/B2: `cargo build`'s
/// stdout/stderr is CAPTURED and routed as `SessionEvent::Diagnostic`s
/// (`ui.command_status_captured`, below) instead of inherited straight
/// through to this process's own stdout/stderr - a raw child write racing an
/// active TUI redraw could corrupt the alternate-screen frame, and under
/// `--message-format json` it could leak onto a stderr the contract promises
/// stays empty. This still reports progress with a single themed (and
/// `--message-format json`-silenced, via `Ui::info`) line rather than an
/// animated spinner - `crate::progress`'s own session-routing (see its
/// module docs) already keeps a spinner from colliding with captured build
/// output, but a single line is simpler here and matches
/// `check::build_and_locate_binary`'s equivalent build.
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
    // Finding A3: a source participant only ever gets here when it genuinely
    // needs a fresh `cargo build` (path-overridden components/simulators, or
    // any user service/driver built from local source) - so bracketing this
    // exact call with a "build" phase reports truthful per-operation work
    // rather than the old synthetic single "Preparing" phase.
    crate::session::diagnostics::run_phase(
        crate::session::event::PhaseId::new("build"),
        format!("Building {preferred_name}"),
        || {
            let mut command = Command::new("cargo");
            command
                .arg("build")
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
    let component = resolved.robot.robot.components.get(participant_id)?;
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
    use crate::launch_plan::{LaunchOwnership, ParticipantLaunchRecord, SITE_TOOL_TELEMETRY};
    use phoxal::participant::launch::{
        BusProfile, ClockMode, DEFAULT_SHUTDOWN_GRACE_MS, ParticipantLaunch,
    };

    #[test]
    fn configless_site_tool_omits_config_and_gets_connect() {
        // A configless tool (phoxal_config == Value::Null, e.g. joypad/telemetry)
        // must NOT receive PHOXAL_CONFIG - a unit config rejects `{}` - and, being
        // a real bus client, MUST receive PHOXAL_CONNECT to reach the router bus.
        let tool = SiteLaunch {
            id: SITE_TOOL_TELEMETRY.to_string(),
            artifact_ref: "phoxal/tool-telemetry@0.1.0".to_string(),
            phoxal_config: serde_json::Value::Null,
        };
        let env = site_env(&tool, "dev", "rover-01").expect("site_env");
        assert!(
            !env.iter().any(|(k, _)| k == env::CONFIG),
            "configless tool must not get PHOXAL_CONFIG: {env:?}"
        );
        assert!(
            env.iter().any(|(k, _)| k == env::CONNECT),
            "observable bus tool must get PHOXAL_CONNECT: {env:?}"
        );
    }

    #[test]
    fn router_site_tool_gets_config_and_no_connect() {
        // The router carries real config and IS the transport, so it gets
        // PHOXAL_CONFIG but no PHOXAL_CONNECT (it does not connect to itself).
        let router = SiteLaunch {
            id: SITE_TOOL_ROUTER.to_string(),
            artifact_ref: "phoxal/tool-router@0.1.8".to_string(),
            phoxal_config: serde_json::json!({ "uplink": null }),
        };
        let env = site_env(&router, "dev", "rover-01").expect("site_env");
        assert!(
            env.iter().any(|(k, _)| k == env::CONFIG),
            "router must get PHOXAL_CONFIG: {env:?}"
        );
        assert!(
            !env.iter().any(|(k, _)| k == env::CONNECT),
            "router must not get PHOXAL_CONNECT: {env:?}"
        );
    }

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
            launch_ownership: LaunchOwnership::CliManaged,
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
                drivers: DriversMode::On,
                drivers_subset: vec!["imu".to_string()],
                catalog_source: None,
                overlays: Vec::new(),
                watch: false,
                message_format: MessageFormat::Human,
                output_mode: crate::output_mode::OutputMode::from_env(),
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
                drivers: DriversMode::On,
                drivers_subset: vec!["missing".to_string()],
                catalog_source: None,
                overlays: Vec::new(),
                watch: false,
                message_format: MessageFormat::Human,
                output_mode: crate::output_mode::OutputMode::from_env(),
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
                drivers: DriversMode::Off,
                drivers_subset: Vec::new(),
                catalog_source: None,
                overlays: Vec::new(),
                watch: false,
                message_format: MessageFormat::Human,
                output_mode: crate::output_mode::OutputMode::from_env(),
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

    /// Scoped `PHOXAL_ARTIFACT_<ID>_PATH` override so `locate_tool_binary` /
    /// `locate_official_binary` resolve without a real `cargo build` or
    /// native-artifact cache. Points at the test binary itself (always a
    /// real file). SAFETY: env mutation only races with another thread also
    /// touching process env; this guard uses a key unique to this test
    /// (`env_key` of a test-only id) and restores the prior (absent) value on
    /// drop, mirroring the existing `WebotsHomeEnvGuard` precedent in
    /// `commands::simulate`.
    struct ArtifactPathEnvGuard {
        key: String,
    }

    impl ArtifactPathEnvGuard {
        fn set(id: &str) -> Self {
            let key = format!("PHOXAL_ARTIFACT_{}_PATH", env_key(id));
            let path = std::env::current_exe().expect("test binary path");
            // SAFETY: scoped to this test via a unique env key, restored on
            // drop before the next test can observe it.
            unsafe {
                std::env::set_var(&key, path);
            }
            Self { key }
        }
    }

    impl Drop for ArtifactPathEnvGuard {
        fn drop(&mut self) {
            // SAFETY: only ever clears the key this guard set.
            unsafe {
                std::env::remove_var(&self.key);
            }
        }
    }

    fn resolved_robot_with_router() -> Result<ResolvedRobot> {
        let yaml = r#"schema: robot/v0
robot:
  id: robot_v1
  namespace: dev
  structure: structure.urdf
  kinematic:
    kind: omnidirectional
    actuators: []
    encoders: []
  components: {}
"#;
        let robot = phoxal::model::robot::v0::Robot::parse_from_string(yaml)?;
        Ok(ResolvedRobot {
            robot,
            channel: crate::catalog::SelectionChannel::Stable,
            target: host_target_triple(),
            catalog_snapshot: None,
            platform_runtimes: Vec::new(),
            simulators: Vec::new(),
            user_runtimes: Vec::new(),
            components: Vec::new(),
            tools: vec![crate::resolver::ResolvedTool {
                name: SITE_TOOL_ROUTER.to_string(),
                package: format!("phoxal/{SITE_TOOL_ROUTER}"),
                requested: "0.1.0".to_string(),
                resolved: "0.1.0".to_string(),
                repo: "phoxal/framework".to_string(),
                asset: format!("{SITE_TOOL_ROUTER}-0.1.0-{}.tar.gz", host_target_triple()),
                binary_name: SITE_TOOL_ROUTER.to_string(),
                sha256: "0".repeat(64),
                url: None,
                size: None,
                published: false,
                path_override: None,
                channel: crate::catalog::SelectionChannel::Stable,
                target: host_target_triple(),
            }],
            path_overrides: Vec::new(),
        })
    }

    /// The router is the bus transport, not a contract-graph participant: it
    /// must come out of `prepare_site_tools` with `bus_participant: false`
    /// (excluded from the readiness barrier's `expected_bus_ids`, marked
    /// `Ready` on spawn), while an ordinary robot participant produced by
    /// `prepare_robot_participants` stays `bus_participant: true`
    /// (heartbeat-gated by the OBSERVED-readiness barrier). This is the fix
    /// for the regression where a managed `tool-router` hung at `Starting`
    /// forever because its heartbeat is structurally unobservable (its
    /// framework runner bus is in-process, disconnected from the zenoh
    /// network it itself serves).
    #[test]
    fn site_tool_is_not_bus_participant_but_robot_participant_is() -> Result<()> {
        let _router_guard = ArtifactPathEnvGuard::set(SITE_TOOL_ROUTER);
        let official_id = "site_tool_test_official_svc";
        let _official_guard = ArtifactPathEnvGuard::set(official_id);

        let resolved = resolved_robot_with_router()?;
        let plan = LaunchPlan {
            mode: LaunchMode::Run,
            site: vec![SiteLaunch {
                id: SITE_TOOL_ROUTER.to_string(),
                artifact_ref: "phoxal/tool-router@0.1.0".to_string(),
                phoxal_config: Value::Null,
            }],
            robots: vec![crate::launch_plan::RobotLaunch {
                id: "robot".to_string(),
                namespace: "dev".to_string(),
                participants: vec![participant(
                    official_id,
                    ParticipantExecution::OfficialArtifact {
                        artifact_ref: "phoxal/service-drive@0.1.0".to_string(),
                    },
                )],
                substitutions: Vec::new(),
            }],
        };
        let board = BoardBackend::new();
        let mut specs = Vec::new();

        prepare_site_tools(
            &plan,
            &resolved,
            &board,
            &mut specs,
            RouterOwnership::Managed,
            &crate::Ui::from_env(),
        )?;
        prepare_robot_participants(
            &plan,
            &resolved,
            Path::new("/tmp/project"),
            &DriverPolicy::drivers_off_for_sim(),
            &board,
            &mut specs,
            &crate::Ui::from_env(),
        )?;

        let router_spec = specs
            .iter()
            .find(|spec| spec.id == SITE_TOOL_ROUTER)
            .expect("router spec present");
        assert!(
            !router_spec.bus_participant,
            "router (bus transport) must not be heartbeat-gated"
        );

        let robot_spec = specs
            .iter()
            .find(|spec| spec.id == official_id)
            .expect("robot participant spec present");
        assert!(
            robot_spec.bus_participant,
            "a real contract-graph participant must stay heartbeat-gated"
        );

        Ok(())
    }

    /// A reused external router (`RouterOwnership::External`) never gets a
    /// `ParticipantSpec` at all - `local_router_reachable` already proved the
    /// transport is up, so it must be marked `Ready` on the board directly,
    /// not left at `Starting` forever (nothing will ever move it out of that
    /// state: it has no spawned process and, being a site tool, is excluded
    /// from the heartbeat-driven barrier too).
    #[test]
    fn external_router_reused_is_marked_ready_not_starting() -> Result<()> {
        let resolved = resolved_robot_with_router()?;
        let plan = LaunchPlan {
            mode: LaunchMode::Run,
            site: vec![SiteLaunch {
                id: SITE_TOOL_ROUTER.to_string(),
                artifact_ref: "phoxal/tool-router@0.1.0".to_string(),
                phoxal_config: Value::Null,
            }],
            robots: Vec::new(),
        };
        let board = BoardBackend::new();
        let mut specs = Vec::new();

        prepare_site_tools(
            &plan,
            &resolved,
            &board,
            &mut specs,
            RouterOwnership::External,
            &crate::Ui::from_env(),
        )?;

        assert!(
            specs.is_empty(),
            "an external router must not get a ParticipantSpec"
        );
        let snapshot = board.snapshot();
        let status = snapshot
            .participants
            .get(SITE_TOOL_ROUTER)
            .expect("router status present on board");
        assert_eq!(status.state, ParticipantState::Ready);
        assert_eq!(status.note.as_deref(), Some("external router reused"));

        Ok(())
    }

    /// The `--message-format json` launch report's `kind` string is a
    /// Phase-0 contract that must stay byte-identical even though the
    /// board's own `ParticipantKind` is now the finer-grained shared
    /// `Tool`/`Service`/`Driver`/`Simulator` split plus a `local` bit (Part
    /// 1 kind consolidation) - see `legacy_launch_kind_label`'s docs.
    #[test]
    fn legacy_launch_kind_label_matches_the_pre_consolidation_strings() {
        assert_eq!(legacy_launch_kind_label(None), "site-tool");
        assert_eq!(
            legacy_launch_kind_label(Some(&ParticipantExecution::OfficialArtifact {
                artifact_ref: "phoxal/service-drive@1.0.0".to_string(),
            })),
            "official"
        );
        assert_eq!(
            legacy_launch_kind_label(Some(&ParticipantExecution::SourceArtifact {
                kind: "service".to_string(),
                crate_dir: PathBuf::from("/tmp/drive"),
            })),
            "official",
            "a locally source-overridden official artifact stayed bucketed as \"official\" pre-consolidation"
        );
        assert_eq!(
            legacy_launch_kind_label(Some(&ParticipantExecution::UserService {
                crate_dir: PathBuf::from("/tmp/mission"),
            })),
            "user-service"
        );
        assert_eq!(
            legacy_launch_kind_label(Some(&ParticipantExecution::ComponentDriver {
                crate_dir: PathBuf::from("/tmp/ddsm115"),
            })),
            "driver"
        );
    }
}
