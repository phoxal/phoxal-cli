//! Live simulation resource assembly and ownership.

use super::{
    SimPlan, SimulateOptions, prepare_substitution_notes, stage_and_prepare_webots_spec,
    stages_for_simulate, start_spawn_responder,
};
use crate::session::output::OutputContext;
use crate::supervisor::BoardBackend;
use crate::supervisor::ProjectLock;
use crate::supervisor::ProjectLockIdentity;
use crate::supervisor::ProjectOperation;
use crate::supervisor::RequestedStop;
use crate::supervisor::SupervisionStage;
use crate::supervisor::SupervisorAction;
use crate::supervisor::SupervisorOptions;
use crate::supervisor::start_bus_log_subscriber;
use crate::supervisor::start_clock_feed;
use crate::supervisor::start_liveliness_observer;
use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use anyhow::bail;
use tokio::sync::mpsc;

/// Everything [`live_simulate_setup`] hands back to the caller once it
/// completes: the board/telemetry/supervisor task `drive_supervision` needs,
/// plus every ancillary task that must be aborted once supervision ends.
pub(crate) struct LiveSimSetup {
    pub(crate) router: crate::run::InfrastructureRouter,
    pub(crate) connect: String,
    // Keep the simulation-specific lease alive for the entire supervision
    // lifetime. The project-operation lock is held by the command from
    // before preparation until this setup and supervision have both ended.
    pub(crate) _locks: LiveSimulationLocks,
    pub(crate) board: BoardBackend,
    pub(crate) telemetry: crate::telemetry::TelemetryBackend,
    pub(crate) runtime_store: phoxal_cli_core::session::stores::runtime::RuntimeStore,
    pub(crate) orderly_shutdown_timeout: std::time::Duration,
    pub(crate) stages: Vec<SupervisionStage>,
    pub(crate) supervisor_options: SupervisorOptions,
    pub(crate) action_tx: mpsc::Sender<SupervisorAction>,
    /// Every feed task that must stay alive for the whole session (log/
    /// Liveliness observers, clock telemetry, live telemetry) -
    /// collected here instead of leaked under `_`-prefixed bindings (finding
    /// B6), so the caller can abort every one of them once supervision ends.
    pub(crate) background_tasks: crate::run::AbortTasks,
}

pub(crate) struct LiveSimulationLocks {
    _simulator_lock: ProjectLock,
}

impl LiveSimulationLocks {
    pub(crate) fn acquire(
        simulator_lock_path: &std::path::Path,
        identity: ProjectLockIdentity,
    ) -> Result<Self> {
        Ok(Self {
            _simulator_lock: ProjectLock::acquire_path(simulator_lock_path, identity)?,
        })
    }
}

/// Everything between preparation finishing and supervision beginning for a
/// live `simulation run`: Webots preflight, lock acquisition, world/
/// controller staging, and spawn-responder startup (finding A1's
/// "intermediate setup" gap), plus starting every feed/watcher task
/// supervision needs. Driven through `SessionController::drive_setup` (see
/// the call site) so Ctrl-C is observed the whole time this runs, not only
/// once it returns.
pub(crate) async fn live_simulate_setup(
    ui: crate::Ui,
    mut sim: SimPlan,
    options: SimulateOptions,
    events: mpsc::Sender<phoxal_cli_core::session::event::SessionEvent>,
    token: tokio_util::sync::CancellationToken,
    output: OutputContext,
    renders_tui: bool,
) -> Result<LiveSimSetup> {
    let ensure_active = || {
        if token.is_cancelled() {
            bail!("simulation setup cancelled");
        }
        Ok(())
    };
    ensure_active()?;
    crate::host_doctor::preflight()
        .map_err(|error| anyhow!("{error}"))
        .context("Webots preflight failed; live simulate cannot launch the simulator")?;
    ensure_active()?;

    let identity = ProjectLockIdentity::resolve(&sim.ctx.project_root, ProjectOperation::Run);
    let locks = LiveSimulationLocks::acquire(&crate::host_paths::simulator_lock_path()?, identity)?;
    crate::stager::stage_runtime_layout(&sim.ctx.project_root, &sim.ctx.resolved)
        .context("failed to stage the simulation runtime layout")?;
    ensure_active()?;
    let board = BoardBackend::new();
    board.configure(
        sim.ctx.project_root.display().to_string(),
        sim.ctx.resolved.train.clone(),
        "simulation",
    );
    board.upsert_process(
        phoxal_cli_core::session::ProcessKey::project("infrastructure-router"),
        crate::supervisor::ParticipantStatus::new(
            "infrastructure-router",
            phoxal_cli_core::session::ParticipantKind::Tool,
            crate::supervisor::ParticipantState::Starting,
        ),
        phoxal_cli_core::session::StartupRequirement::Required,
    );
    let runtime_store = sim.runtime_store.clone();
    let mut specs = Vec::new();
    ensure_active()?;
    crate::run::prepare_robot_participants(
        &sim.plan,
        &sim.ctx.resolved,
        &sim.ctx.project_root,
        &crate::run::DriverPolicy::drivers_off_for_sim(),
        &board,
        &mut specs,
        &ui,
    )?;
    let (router, connect) =
        crate::run::start_infrastructure_router(&sim.ctx.resolved, &sim.ctx.project_root, &ui)
            .await?;
    board.set_router_status(format!("ready:{connect}"));
    board.set_state(
        phoxal_cli_core::session::ProcessKey::project("infrastructure-router"),
        crate::supervisor::ParticipantState::Ready,
        None,
    );
    crate::run::apply_session_connect(&mut sim.plan, &mut specs, &connect);
    ensure_active()?;
    prepare_substitution_notes(&sim.plan, &board);

    let (webots_spec, spawn_descriptors) = stage_and_prepare_webots_spec(&ui, &sim)?;
    ensure_active()?;
    let mut background_tasks = crate::run::AbortTasks::default();
    let spawn_responder = start_spawn_responder(&sim.plan, spawn_descriptors, &connect).await?;
    background_tasks.push(spawn_responder);
    ensure_active()?;
    let requested_stop = RequestedStop::new(webots_spec.key.clone(), webots_spec.shutdown_grace);
    specs.push(webots_spec);
    let revision =
        phoxal_cli_core::project::launch_plan::PlanRevision::compile(1, sim.plan.clone())?;
    crate::supervisor::materialize_plan_binaries(&sim.ctx.project_root, &revision, &mut specs)?;

    ui.info(format!(
        "simulation launch plan resolved: {} robot(s)",
        sim.plan.robots.len()
    ));
    ui.info(format!("infrastructure router ready on {connect}"));
    crate::run::report_launch_commands(&sim.plan, &specs, &ui)?;

    background_tasks.extend(
        sim.plan
            .robots
            .iter()
            .map(|robot| {
                start_bus_log_subscriber(
                    robot.namespace.clone(),
                    robot.id.clone(),
                    connect.clone(),
                    board.clone(),
                )
            })
            .collect::<Vec<_>>(),
    );
    // OBSERVED readiness: drive board state from each participant's own
    // Liveliness token, including SIMULATION-MANAGED ones (the
    // supervisor and every controller), which have no supervised
    // process of their own to poll.
    background_tasks.extend(sim.plan.robots.iter().map(|robot| {
        start_liveliness_observer(
            robot.namespace.clone(),
            robot.id.clone(),
            connect.clone(),
            board.clone(),
        )
    }));
    let clock_robot = sim
        .plan
        .robots
        .first()
        .context("sim launch plan has no robot for the clock telemetry feed")?;
    let (clock_rx, clock_task) = start_clock_feed(
        clock_robot.namespace.clone(),
        clock_robot.id.clone(),
        connect.clone(),
    );
    background_tasks.push(clock_task);
    // Clock observation is telemetry only. Startup and session state do not
    // wait for a sample; clocked services and drivers consume it independently
    // through their simulation-clock runner policy.
    let telemetry = crate::telemetry::TelemetryBackend::new();
    telemetry.set_clock_feed(clock_rx.clone());

    // The restart/hot-reload action channel always exists now (not just
    // under `--watch`), matching `crate::run`.
    let (action_tx, action_rx) = mpsc::channel(16);
    if options.watch {
        let live_ids = specs
            .iter()
            .map(|spec| spec.id.clone())
            .collect::<std::collections::BTreeSet<_>>();
        background_tasks.push(crate::watch::spawn_sim_watch(
            crate::watch::SimWatchConfig {
                ctx: sim.ctx.clone(),
                options: options.clone(),
                live_ids,
                board: board.clone(),
                action_tx: action_tx.clone(),
            },
        ));
    }

    let stages = stages_for_simulate(specs, &sim.plan, output);

    // Live telemetry (CLI-UX Phase 3/4): only worth subscribing when
    // a real TUI is up to read it, same gate as `crate::run`. The
    // sim clock feed (`telemetry.set_clock_feed` above) is wired
    // unconditionally since it costs nothing extra - the SAME task
    // already exists for the title telemetry - but device/runtime/
    // router/joypad each open their own bus connection, so those
    // stay Tui-gated.
    let feed_targets = crate::run::RobotFeedTarget::from_plan(&sim.plan);
    if renders_tui {
        background_tasks.extend(crate::run::start_telemetry_feeds_at(
            &feed_targets,
            &telemetry,
            &connect,
            board.recovery_epoch_receiver(),
        ));
    }

    let starting = phoxal_cli_core::session::state::SessionState::Preparing
        .start()
        .expect("the controller begins every session in Preparing");
    let _ = events
        .send(phoxal_cli_core::session::event::SessionEvent::SessionChanged { state: starting })
        .await;
    let supervisor_options = SupervisorOptions {
        action_rx: Some(crate::supervisor::SupervisorActionReceiver::new(action_rx)),
        requested_stop: Some(requested_stop),
        token: token.clone(),
        events: Some(events.clone()),
        emits_running_on_startup_complete: true,
    };

    let orderly_shutdown_timeout = crate::supervisor::orderly_shutdown_budget(&stages);
    Ok(LiveSimSetup {
        router,
        connect,
        _locks: locks,
        board,
        telemetry,
        runtime_store,
        orderly_shutdown_timeout,
        stages,
        supervisor_options,
        action_tx,
        background_tasks,
    })
}
