use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use clap::{Args, ValueEnum};
use phoxal::model::robot::v0::ConnectionConfig;
use phoxal::participant::launch::env;
use serde::Deserialize;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc;

use crate::AppContext;
use crate::commands::check::{
    CheckGraphContext, build_emit_apis_from_source, check_artifact_refs_from_resolved,
    extract_emit_apis_from_staged_runtime, extract_emit_apis_from_staged_tool,
    fetch_emit_apis_from_tool, run_check_with_context, source_participants_from_resolved,
    tool_participants_from_resolved,
};
use crate::component_driver::component_driver_crate_dir;
use crate::launch_plan::{
    CheckedRobotLaunchInput, LaunchMode, LaunchOwnership, LaunchPlan, ParticipantExecution,
    ParticipantLaunchRecord, PlanContext, SITE_INFRASTRUCTURE_ROUTER, SITE_TOOL_JOYPAD, SiteLaunch,
    build_launch_plan,
};
use crate::resolver::{
    ResolveOptions, ResolvedPlatformRuntime, ResolvedRobot, discover_robot_yaml,
    host_target_triple, load_robot_with_extras, resolve,
};
use crate::supervisor::{
    BoardBackend, ParticipantSpec, ParticipantState, ParticipantStatus, SupervisionStage,
    SupervisorLock, SupervisorOptions, SupervisorOutcome, default_connect_endpoint,
    start_bus_log_subscriber, start_presence_heartbeat_subscriber, supervise_until_shutdown,
};
use phoxal_cli_core::project::tooling::{cargo_binary_name, resolve_project_path};
use phoxal_cli_core::session::ParticipantKind;
use phoxal_cli_core::session::launch_env::encode_participant_env;

/// How long a `run` staged-startup stage may wait for its members to be
/// OBSERVED ready before the whole run fails naming the stalled stage - see
/// `stages_for_run` and `supervisor::SupervisionStage`. Every `run`
/// participant is CLI-managed and expected to clear its own `#[setup]`
/// quickly on a loaded host; generous enough to absorb ordinary scheduling
/// jitter without masking a genuinely hung participant.
const RUN_STAGE_READY_TIMEOUT: Duration = Duration::from_secs(60);
const ROUTER_READY_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug, Deserialize)]
struct RouterReadyEvent {
    event: String,
    listen: Vec<String>,
}

pub(crate) struct InfrastructureRouter {
    child: tokio::process::Child,
    stdout_task: tokio::task::JoinHandle<()>,
    stderr_task: tokio::task::JoinHandle<()>,
}

impl InfrastructureRouter {
    async fn stop(mut self) {
        if let Some(pid) = self.child.id() {
            // SAFETY: `pid` is the live child id returned by Tokio. SIGTERM
            // lets the router close its Zenoh session before the bounded
            // fallback below forces termination.
            let _ = unsafe { libc::kill(pid as i32, libc::SIGTERM) };
        }
        if tokio::time::timeout(Duration::from_secs(5), self.child.wait())
            .await
            .is_err()
        {
            let _ = self.child.kill().await;
            let _ = self.child.wait().await;
        }
        self.stdout_task.abort();
        self.stderr_task.abort();
    }

    pub(crate) async fn supervise(
        mut self,
        stages: Vec<SupervisionStage>,
        board: BoardBackend,
        options: SupervisorOptions,
    ) -> Result<SupervisorOutcome> {
        let token = options.token.clone();
        let supervisor = supervise_until_shutdown(stages, board, options);
        tokio::pin!(supervisor);
        tokio::select! {
            outcome = &mut supervisor => {
                self.stop().await;
                outcome
            }
            status = self.child.wait() => {
                token.cancel();
                let _ = supervisor.await;
                self.stdout_task.abort();
                self.stderr_task.abort();
                let status = status.context("failed to wait for infrastructure router")?;
                bail!("infrastructure router exited while the session was active: {status}")
            }
        }
    }
}

pub(crate) async fn start_infrastructure_router(
    resolved: &ResolvedRobot,
    project_root: &Path,
    ui: &crate::Ui,
) -> Result<(InfrastructureRouter, String)> {
    let binary = locate_tool_binary(resolved, SITE_INFRASTRUCTURE_ROUTER, ui)?
        .context("phoxal-infrastructure-router is not staged; run `phoxal update`")?;
    let mut command = tokio::process::Command::new(binary);
    if let Some(config) = &resolved.robot.router.config {
        let config = resolve_project_path(project_root, config);
        anyhow::ensure!(
            config.is_file(),
            "router.config file {} does not exist",
            config.display()
        );
        command.arg("--config").arg(config);
    }
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let mut child = command
        .spawn()
        .context("failed to launch phoxal-infrastructure-router")?;
    let stdout = child
        .stdout
        .take()
        .context("router stdout was not captured")?;
    let stderr = child
        .stderr
        .take()
        .context("router stderr was not captured")?;
    let stderr_tail = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    let task_stderr_tail = std::sync::Arc::clone(&stderr_tail);
    let stderr_task = tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            tracing::info!(target: "infrastructure_router", "{line}");
            if let Ok(mut tail) = task_stderr_tail.lock() {
                tail.push_str(&line);
                tail.push('\n');
                if tail.len() > 8_192 {
                    let mut drain = tail.len() - 8_192;
                    while !tail.is_char_boundary(drain) {
                        drain += 1;
                    }
                    tail.drain(..drain);
                }
            }
        }
    });
    let mut lines = BufReader::new(stdout).lines();
    let readiness = tokio::time::timeout(ROUTER_READY_TIMEOUT, async {
        loop {
            let line = lines
                .next_line()
                .await?
                .context("infrastructure router exited before reporting readiness")?;
            let event: RouterReadyEvent = serde_json::from_str(&line)
                .with_context(|| format!("invalid infrastructure router event: {line}"))?;
            if event.event == "ready" {
                break parse_router_ready(&line);
            }
            tracing::info!(target: "infrastructure_router", "{line}");
        }
    })
    .await
    .context("timed out waiting for infrastructure router readiness")?;
    let endpoint = match readiness {
        Ok(endpoint) => endpoint,
        Err(error) => {
            let _ = tokio::time::timeout(Duration::from_millis(100), async {
                while stderr_tail.lock().is_ok_and(|tail| tail.is_empty()) {
                    tokio::task::yield_now().await;
                }
            })
            .await;
            let tail = stderr_tail
                .lock()
                .map(|tail| tail.clone())
                .unwrap_or_default();
            if tail.is_empty() {
                return Err(error);
            }
            return Err(error.context(format!("infrastructure router stderr:\n{tail}")));
        }
    };
    let stdout_task = tokio::spawn(async move {
        while let Ok(Some(line)) = lines.next_line().await {
            tracing::info!(target: "infrastructure_router", "{line}");
        }
    });
    Ok((
        InfrastructureRouter {
            child,
            stdout_task,
            stderr_task,
        },
        endpoint,
    ))
}

fn parse_router_ready(line: &str) -> Result<String> {
    let ready: RouterReadyEvent = serde_json::from_str(line)
        .with_context(|| format!("invalid infrastructure router readiness event: {line}"))?;
    anyhow::ensure!(
        ready.event == "ready",
        "unexpected router event {}",
        ready.event
    );
    ready
        .listen
        .first()
        .cloned()
        .context("infrastructure router reported no listener endpoint")
}

pub(crate) fn apply_session_connect(
    plan: &mut LaunchPlan,
    specs: &mut [ParticipantSpec],
    endpoint: &str,
) {
    for robot in &mut plan.robots {
        for participant in &mut robot.participants {
            participant.launch.bus.connect_endpoints = vec![endpoint.to_string()];
        }
    }
    for spec in specs {
        if let Some((_, value)) = spec.env.iter_mut().find(|(key, _)| key == env::CONNECT) {
            *value = endpoint.to_string();
        }
    }
}

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
}

#[derive(Debug)]
struct PreparedRun {
    ctx: PlanContext,
    plan: LaunchPlan,
    board: BoardBackend,
    specs: Vec<ParticipantSpec>,
    robot_log_targets: Vec<(String, String)>,
    /// Finding A5: this session's launch-time participant metadata, resolved
    /// once here from `plan` and the contract-check `outcome` - see
    /// `crate::stores::runtime_store::RuntimeStore`'s own docs.
    runtime_store: crate::stores::runtime_store::RuntimeStore,
}

/// Resources assembled after preparation but before the controller enters
/// supervision. Keeping this whole phase behind `drive_setup` means raw-mode
/// Ctrl-C remains polled until the supervisor loop takes ownership.
struct LiveRunSetup {
    router: InfrastructureRouter,
    connect: String,
    board: BoardBackend,
    telemetry: crate::telemetry::TelemetryBackend,
    runtime_store: crate::stores::runtime_store::RuntimeStore,
    orderly_shutdown_timeout: Duration,
    stages: Vec<SupervisionStage>,
    supervisor_options: SupervisorOptions,
    background_tasks: AbortTasks,
    action_tx: mpsc::Sender<crate::supervisor::SupervisorAction>,
}

#[derive(Default)]
pub(super) struct AbortTasks {
    handles: Vec<tokio::task::JoinHandle<()>>,
}

impl AbortTasks {
    pub(super) fn push(&mut self, handle: tokio::task::JoinHandle<()>) {
        self.handles.push(handle);
    }

    pub(super) fn extend(
        &mut self,
        handles: impl IntoIterator<Item = tokio::task::JoinHandle<()>>,
    ) {
        self.handles.extend(handles);
    }
}

impl Drop for AbortTasks {
    fn drop(&mut self) {
        for handle in &self.handles {
            handle.abort();
        }
    }
}

impl Run {
    pub async fn run(&self, app: &AppContext) -> Result<()> {
        let options = RunOptions {
            drivers: self.drivers,
            drivers_subset: self.drivers_subset.clone(),
            catalog_source: app.catalog_source.clone(),
            overlays: self.env.clone(),
            watch: self.watch,
        };
        if options.drivers == DriversMode::Off && !options.drivers_subset.is_empty() {
            bail!("--driver cannot be combined with --drivers off");
        }
        let watch_enabled = options.watch;
        let watch_options = options.clone();

        let run_dir = crate::host_paths::run_dir()?;
        let _lock = SupervisorLock::acquire(&run_dir)?;
        let project_root = app.project.root().to_path_buf();
        let ui = app.ui;

        // One interactive surface for the whole session (Product decision
        // 1): the controller starts the TUI's alternate screen right now,
        // before preparation
        // even begins - see `SessionController::new`'s docs.
        let mut controller = crate::session::controller::SessionController::new(
            app.output,
            crate::session::controller::SessionMode::Run,
            app.project.root(),
        )?;
        let events = controller.events();

        let prepared = controller
            .drive_prepare_phase(move || prepare_run(&project_root, options, &ui))
            .await?;

        let setup = controller
            .drive_setup(live_run_setup(
                prepared,
                app.ui,
                watch_enabled,
                watch_options,
                controller.output(),
                controller.token(),
                events,
                controller.renders_tui(),
            ))
            .await?;
        let LiveRunSetup {
            router,
            connect,
            board,
            telemetry,
            runtime_store,
            orderly_shutdown_timeout,
            stages,
            supervisor_options,
            background_tasks,
            action_tx,
        } = setup;
        controller.set_bus_endpoint(connect);
        controller.set_restart_channel(action_tx);
        // Start process supervision only after `drive_setup` has returned its
        // owned result. A cancellation racing the end of setup can therefore
        // never discard a freshly spawned supervisor JoinHandle.
        let supervise_task =
            tokio::spawn(router.supervise(stages, board.clone(), supervisor_options));

        let outcome = controller
            .drive_supervision(
                board,
                telemetry,
                runtime_store,
                orderly_shutdown_timeout,
                supervise_task,
            )
            .await;
        drop(background_tasks);
        let outcome = outcome?;
        // `drive_supervision` consumes and tears down the controller before
        // returning. During the session the same failures stay visible on the
        // board; they are status, never command failure.
        if !outcome.failed_participants.is_empty() {
            app.ui.warn(format!(
                "session stopped with failed participants: {}",
                outcome.failed_participants.join(", ")
            ));
        }
        Ok(())
    }
}

#[allow(clippy::too_many_arguments)]
async fn live_run_setup(
    mut prepared: PreparedRun,
    ui: crate::Ui,
    watch_enabled: bool,
    watch_options: RunOptions,
    output: crate::session::output::OutputContext,
    token: tokio_util::sync::CancellationToken,
    events: mpsc::Sender<crate::session::event::SessionEvent>,
    renders_tui: bool,
) -> Result<LiveRunSetup> {
    let (router, connect) =
        start_infrastructure_router(&prepared.ctx.resolved, &prepared.ctx.project_root, &ui)
            .await?;
    apply_session_connect(&mut prepared.plan, &mut prepared.specs, &connect);
    ui.info(format!(
        "launch plan resolved: {} robot(s), {} site tool(s)",
        prepared.plan.robots.len(),
        prepared.plan.site.len()
    ));
    ui.info(format!("infrastructure router ready on {connect}"));
    report_launch_commands(&prepared.plan, &prepared.specs, &ui)?;

    let mut background_tasks = AbortTasks::default();
    background_tasks.extend(
        prepared
            .robot_log_targets
            .iter()
            .map(|(namespace, robot_id)| {
                start_bus_log_subscriber(
                    namespace.clone(),
                    robot_id.clone(),
                    connect.clone(),
                    prepared.board.clone(),
                )
            })
            .collect::<Vec<_>>(),
    );
    background_tasks.extend(
        prepared
            .robot_log_targets
            .iter()
            .map(|(namespace, robot_id)| {
                start_presence_heartbeat_subscriber(
                    namespace.clone(),
                    robot_id.clone(),
                    connect.clone(),
                    prepared.board.clone(),
                )
            }),
    );

    let (action_tx, action_rx) = mpsc::channel(16);
    if watch_enabled {
        let live_ids = prepared
            .specs
            .iter()
            .map(|spec| spec.id.clone())
            .collect::<BTreeSet<_>>();
        background_tasks.push(crate::watch::spawn_run_watch(
            crate::watch::RunWatchConfig {
                ctx: prepared.ctx.clone(),
                options: watch_options,
                live_ids,
                board: prepared.board.clone(),
                action_tx: action_tx.clone(),
            },
        ));
    }

    let stages = stages_for_run(prepared.specs, output);
    let starting = crate::session::state::SessionState::Preparing
        .start()
        .expect("the controller begins every session in Preparing");
    let _ = events
        .send(crate::session::event::SessionEvent::SessionChanged { state: starting })
        .await;

    let telemetry = crate::telemetry::TelemetryBackend::new();
    if renders_tui {
        background_tasks.extend(start_telemetry_feeds_at(
            &prepared.robot_log_targets,
            &telemetry,
            &connect,
        ));
    }

    let board = prepared.board;
    let supervisor_options = SupervisorOptions {
        action_rx: Some(action_rx),
        token,
        events: Some(events),
        emits_running_on_startup_complete: true,
        ..SupervisorOptions::default()
    };

    let orderly_shutdown_timeout = crate::supervisor::orderly_shutdown_budget(&stages);
    Ok(LiveRunSetup {
        router,
        connect,
        board,
        telemetry,
        runtime_store: prepared.runtime_store,
        orderly_shutdown_timeout,
        stages,
        supervisor_options,
        background_tasks,
        action_tx,
    })
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
    let mut tools = Vec::new();
    let mut drivers = Vec::new();
    let mut services = Vec::new();
    for spec in specs {
        match spec.kind {
            ParticipantKind::Tool => tools.push(spec),
            ParticipantKind::Driver => drivers.push(spec),
            ParticipantKind::Service | ParticipantKind::Simulator => services.push(spec),
        }
    }
    // Product decision 6: no unconditional 60s teardown for an interactive
    // session - see `OutputContext::wait_budget`.
    let timeout = output.wait_budget(RUN_STAGE_READY_TIMEOUT);
    vec![
        SupervisionStage::new("starting tools", tools, timeout),
        SupervisionStage::new("starting drivers", drivers, timeout),
        SupervisionStage::new("starting services", services, timeout),
    ]
}

/// Start the host/router-metrics/joypad-devices telemetry feeds
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
pub(crate) fn start_telemetry_feeds_at(
    robot_log_targets: &[(String, String)],
    telemetry: &crate::telemetry::TelemetryBackend,
    connect: &str,
) -> Vec<tokio::task::JoinHandle<()>> {
    let Some((namespace, robot_id)) = robot_log_targets.first() else {
        return Vec::new();
    };
    vec![
        crate::telemetry::start_host_feed(
            namespace.clone(),
            robot_id.clone(),
            connect.to_string(),
            telemetry.clone(),
        ),
        crate::telemetry::start_router_metrics_feed(
            namespace.clone(),
            robot_id.clone(),
            connect.to_string(),
            telemetry.clone(),
        ),
        crate::telemetry::start_joypad_devices_feed(
            namespace.clone(),
            robot_id.clone(),
            connect.to_string(),
            telemetry.clone(),
        ),
        crate::telemetry::start_control_state_feed(
            namespace.clone(),
            robot_id.clone(),
            connect.to_string(),
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
    )?;
    let resolved = resolve(
        &loaded.robot,
        project_root,
        catalog.as_ref(),
        ResolveOptions {
            resolve_source_commits: true,
            resolve_component_asset_commits: false,
            ..ResolveOptions::default()
        },
    )?;
    let descriptors = crate::native_artifacts::descriptors_for(&resolved, false, true)?;
    crate::native_artifacts::prepare_descriptors_with_preflight(&descriptors, Some(ui))?;
    let runtime_root = crate::runtime_root::publish(project_root, &resolved)
        .context("failed to publish the runtime robot root")?;

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
    )?;
    // Finding A5: resolved once here, from the same `plan`/`outcome` this
    // function already built - see `RuntimeStore::from_launch_plan`'s docs.
    let runtime_store = crate::stores::runtime_store::RuntimeStore::from_launch_plan(
        &plan,
        &outcome.contract_surfaces,
    );
    let board = BoardBackend::new();
    let mut specs = Vec::new();

    prepare_site_tools(&plan, &resolved, &runtime_root, &board, &mut specs, ui)?;
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
        runtime_store,
    })
}

#[derive(Debug)]
struct LaunchCommandReport {
    participants: Vec<LaunchCommandEntry>,
}

#[derive(Debug)]
struct LaunchCommandEntry {
    id: String,
    kind: &'static str,
    command_line: String,
}

/// The pre-staged-startup launch-report `kind` string. The board's
/// own `ParticipantKind` is the finer-grained shared
/// `Tool`/`Service`/`Driver`/`Simulator` split plus a `local` bit (Part 1) -
/// see `phoxal_cli_core::session::participant_kind`'s module docs. A site
/// launch (the router, the
/// joypad, the Webots app in `simulate`) has no `ParticipantExecution` of
/// its own and is always `"site-tool"`; everything else follows the
/// compact operator-facing mapping.
fn launch_kind_label(execution: Option<&ParticipantExecution>) -> &'static str {
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
    ui: &crate::Ui,
) -> Result<()> {
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
                    kind: launch_kind_label(executions_by_id.get(spec.id.as_str()).copied()),
                    command_line: launch.command_line,
                }
            })
            .collect(),
    };
    report_launch_commands_human(&output, ui)
}

fn report_launch_commands_human(output: &LaunchCommandReport, ui: &crate::Ui) -> Result<()> {
    // A live session already owns stdout/stderr, so every human line must
    // enter its diagnostic stream instead of racing the TUI's alternate-
    // screen redraw. `Ui` retains the ordinary raw-mode fallback when this
    // helper is ever used outside a session.
    ui.info("resolved launch participants:");
    for participant in &output.participants {
        ui.info(format!(
            "  - {} ({}) -> {}",
            participant.id, participant.kind, participant.command_line
        ));
    }
    ui.info(
        "motion guarantees: e-stop, source freshness, finite values, and robot-authored limits; autonomous motion also requires fresh typed safety constraints",
    );
    Ok(())
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
    robot_root: &Path,
    board: &BoardBackend,
    specs: &mut Vec<ParticipantSpec>,
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
        let status =
            ParticipantStatus::new(&site.id, ParticipantKind::Tool, ParticipantState::Starting)
                .with_local(site_tool_is_local(resolved, &site.id));
        board.upsert(status);
        match locate_tool_binary(resolved, &site.id, ui)? {
            Some(path) => specs.push(ParticipantSpec {
                id: site.id.clone(),
                kind: ParticipantKind::Tool,
                executable: path,
                args: Vec::new(),
                cwd: None,
                env: site_env(site, namespace, robot_id, robot_root)?,
                shutdown_grace: Duration::from_secs(5),
                process_group: true,
                note: None,
                bus_participant: true,
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
                // reaches `Ready` here, and its staged participant wait (or,
                // failing that, the heartbeat staleness sweep) turns that into a
                // detected failure instead of a permanently green board.
                // `commands::simulate` renders its controllerArgs into the
                // staged world instead of a `ParticipantSpec` (Part 5).
                board.mark_presence_recoverable(&id);
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
                            executable: path,
                            args: Vec::new(),
                            cwd: None,
                            env: encode_participant_env(&participant.launch)?.spawn_env(),
                            shutdown_grace: Duration::from_millis(
                                participant.launch.shutdown_grace_ms,
                            ),
                            process_group: true,
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
                        executable: binary,
                        args: Vec::new(),
                        cwd: Some(crate_dir.clone()),
                        env: encode_participant_env(&participant.launch)?.spawn_env(),
                        shutdown_grace: Duration::from_millis(participant.launch.shutdown_grace_ms),
                        process_group: true,
                        note: None,
                        bus_participant: true,
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
                        process_group: true,
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
                        executable: binary,
                        args: Vec::new(),
                        cwd: Some(crate_dir.clone()),
                        env: encode_participant_env(&participant.launch)?.spawn_env(),
                        shutdown_grace: Duration::from_millis(participant.launch.shutdown_grace_ms),
                        process_group: true,
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
    // `_local`: this function only builds a `ParticipantSpec` (no
    // `ParticipantStatus` to mark `.with_local` on) - see the other
    // `participant_kind` call sites for where the bool is actually consumed.
    let (kind, _local) = participant_kind(&participant.execution);
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
        process_group: true,
        note: None,
        bus_participant: true,
    }))
}

fn site_env(
    site: &SiteLaunch,
    namespace: &str,
    robot_id: &str,
    robot_root: &Path,
) -> Result<Vec<(String, String)>> {
    let mut envs = vec![
        (env::PARTICIPANT_ID.to_string(), site.id.clone()),
        (env::NAMESPACE.to_string(), namespace.to_string()),
        (env::ROBOT_ID.to_string(), robot_id.to_string()),
    ];
    if site.id == SITE_TOOL_JOYPAD {
        envs.push((
            env::ROBOT_ROOT.to_string(),
            robot_root.display().to_string(),
        ));
    }
    // A configless tool (`phoxal_config == Value::Null`)
    // must run with `PHOXAL_CONFIG` ABSENT: a unit config (`type Config = ()`)
    // fails to deserialize `{}` ("invalid type: map, expected unit"), and an
    // absent var uses the runner's null/unit fallback.
    if !site.phoxal_config.is_null() {
        envs.push((
            env::CONFIG.to_string(),
            serde_json::to_string(&site.phoxal_config)
                .with_context(|| format!("failed to encode PHOXAL_CONFIG for {}", site.id))?,
        ));
    }
    envs.push((env::CONNECT.to_string(), default_connect_endpoint()));
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
/// active TUI redraw could corrupt the alternate-screen frame. This still
/// reports progress with a single themed line. Session routing keeps that
/// line from colliding with captured build output and matches
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
    use crate::launch_plan::{
        LaunchOwnership, ParticipantLaunchRecord, SITE_TOOL_TELEMETRY, STANDARD_SITE_TOOLS,
    };
    use phoxal::participant::launch::{
        BusProfile, ClockMode, DEFAULT_SHUTDOWN_GRACE_MS, ParticipantLaunch,
    };

    #[test]
    fn human_launch_report_enters_the_active_session_diagnostics() -> Result<()> {
        let _guard = crate::session::diagnostics::DIAGNOSTICS_TEST_LOCK.blocking_lock();
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        crate::session::diagnostics::install(tx);
        let output = LaunchCommandReport {
            participants: vec![LaunchCommandEntry {
                id: "drive".to_string(),
                kind: "official",
                command_line: "service-drive".to_string(),
            }],
        };

        let result = report_launch_commands_human(&output, &crate::Ui::new(true));
        crate::session::diagnostics::uninstall();
        result?;

        let messages = std::iter::from_fn(|| rx.try_recv().ok())
            .filter_map(|event| match event {
                crate::session::event::SessionEvent::Diagnostic { message, .. } => Some(message),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0], "resolved launch participants:");
        assert!(messages[1].contains("drive (official) -> service-drive"));
        assert!(messages[2].starts_with("motion guarantees:"));
        Ok(())
    }

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
        let env = site_env(&tool, "dev", "rover-01", Path::new("/tmp/robot")).expect("site_env");
        assert!(
            !env.iter().any(|(k, _)| k == env::CONFIG),
            "configless tool must not get PHOXAL_CONFIG: {env:?}"
        );
        assert!(
            env.iter().any(|(k, _)| k == env::CONNECT),
            "observable bus tool must get PHOXAL_CONNECT: {env:?}"
        );
        assert!(
            !env.iter().any(|(k, _)| k == env::ROBOT_ROOT),
            "telemetry does not need the compiled robot root: {env:?}"
        );
        assert!(
            !env.iter().any(|(key, _)| key == env::CLOCK),
            "tools must not receive a clock selection: {env:?}"
        );
    }

    #[test]
    fn joypad_receives_the_compiled_robot_root() {
        let tool = SiteLaunch {
            id: SITE_TOOL_JOYPAD.to_string(),
            artifact_ref: "phoxal/tool-joypad@0.1.0".to_string(),
            phoxal_config: serde_json::Value::Null,
        };
        let env = site_env(&tool, "dev", "rover-01", Path::new("/tmp/robot")).expect("site_env");
        assert!(
            env.iter()
                .any(|(key, value)| key == env::ROBOT_ROOT && value == "/tmp/robot"),
            "joypad needs the compiled robot model: {env:?}"
        );
    }

    #[test]
    fn every_standard_site_tool_uses_the_clockless_launch_path() {
        for tool_id in STANDARD_SITE_TOOLS {
            let tool = SiteLaunch {
                id: (*tool_id).to_string(),
                artifact_ref: format!("phoxal/{tool_id}@0.1.0"),
                phoxal_config: serde_json::Value::Null,
            };
            let env =
                site_env(&tool, "dev", "rover-01", Path::new("/tmp/robot")).expect("site_env");
            assert!(
                !env.iter().any(|(key, _)| key == env::CLOCK),
                "{tool_id} must not receive a clock selection: {env:?}"
            );
        }
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

    /// The launch report's `kind` string stays byte-identical even though the
    /// board's own `ParticipantKind` is now the finer-grained shared
    /// `Tool`/`Service`/`Driver`/`Simulator` split plus a `local` bit (Part
    /// 1 kind consolidation) - see `launch_kind_label`'s docs.
    #[test]
    fn launch_kind_label_matches_the_operator_facing_strings() {
        assert_eq!(launch_kind_label(None), "site-tool");
        assert_eq!(
            launch_kind_label(Some(&ParticipantExecution::OfficialArtifact {
                artifact_ref: "phoxal/service-drive@1.0.0".to_string(),
            })),
            "official"
        );
        assert_eq!(
            launch_kind_label(Some(&ParticipantExecution::SourceArtifact {
                kind: "service".to_string(),
                crate_dir: PathBuf::from("/tmp/drive"),
            })),
            "official",
            "a locally source-overridden official artifact stayed bucketed as \"official\" pre-consolidation"
        );
        assert_eq!(
            launch_kind_label(Some(&ParticipantExecution::UserService {
                crate_dir: PathBuf::from("/tmp/mission"),
            })),
            "user-service"
        );
        assert_eq!(
            launch_kind_label(Some(&ParticipantExecution::ComponentDriver {
                crate_dir: PathBuf::from("/tmp/ddsm115"),
            })),
            "driver"
        );
    }

    #[tokio::test]
    async fn dropping_setup_background_tasks_aborts_every_handle() {
        let handle = tokio::spawn(std::future::pending::<()>());
        let abort = handle.abort_handle();
        let mut tasks = AbortTasks::default();
        tasks.push(handle);
        drop(tasks);
        tokio::task::yield_now().await;
        assert!(abort.is_finished());
    }

    #[test]
    fn router_ready_event_selects_first_listener_and_tolerates_additive_fields() {
        assert_eq!(
            parse_router_ready(
                r#"{"event":"ready","listen":["tcp/127.0.0.1:7448"],"future":true}"#
            )
            .expect("ready event"),
            "tcp/127.0.0.1:7448"
        );
        assert!(parse_router_ready(r#"{"event":"ready","listen":[]}"#).is_err());
        assert!(parse_router_ready(r#"{"event":"starting","listen":["tcp/x"]}"#).is_err());
    }

    #[test]
    fn selected_router_endpoint_reaches_plan_and_spawn_environment() {
        let mut plan = plan_with_drivers(&["imu"]);
        let mut specs = vec![ParticipantSpec {
            id: "tool-bus".to_string(),
            kind: ParticipantKind::Tool,
            executable: PathBuf::from("/tmp/tool-bus"),
            args: Vec::new(),
            cwd: None,
            env: vec![(
                env::CONNECT.to_string(),
                crate::launch_plan::DEFAULT_ROUTER_CONNECT.to_string(),
            )],
            shutdown_grace: Duration::from_secs(1),
            process_group: true,
            note: None,
            bus_participant: true,
        }];

        apply_session_connect(&mut plan, &mut specs, "tcp/127.0.0.1:7448");

        assert!(
            plan.robots
                .iter()
                .flat_map(|robot| &robot.participants)
                .all(|participant| participant.launch.bus.connect_endpoints
                    == ["tcp/127.0.0.1:7448"])
        );
        assert_eq!(
            specs[0]
                .env
                .iter()
                .find(|(key, _)| key == env::CONNECT)
                .map(|(_, value)| value.as_str()),
            Some("tcp/127.0.0.1:7448")
        );
    }
}
