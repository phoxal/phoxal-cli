//! Command responsibilities for run.

use super::RobotFeedTarget;
use super::{
    InfrastructureRouter, apply_session_connect, prepare_layout_run_on_board, prepare_run_on_board,
    report_launch_commands, stages_for_run, start_infrastructure_router,
};
use crate::AppContext;
use crate::supervisor::BoardBackend;
use crate::supervisor::ParticipantSpec;
use crate::supervisor::ProjectLock;
use crate::supervisor::ProjectLockIdentity;
use crate::supervisor::ProjectOperation;
use crate::supervisor::SupervisionStage;
use crate::supervisor::SupervisorOptions;
use crate::supervisor::start_bus_log_subscriber;
use crate::supervisor::start_liveliness_observer;
use anyhow::bail;
use anyhow::{Context, Result};
use clap::Args;
use clap::ValueEnum;
use phoxal_cli_core::project::launch_plan::LaunchPlan;
use phoxal_cli_core::project::launch_plan::PlanContext;
use phoxal_cli_core::project::layout::RuntimeLayout;
use phoxal_cli_core::project::train::resolve_locked_train;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;
use tokio::sync::mpsc;

const PREPARATION_CANCEL_TIMEOUT: Duration = if cfg!(test) {
    Duration::from_millis(25)
} else {
    Duration::from_secs(5)
};

#[derive(Debug, Args)]
pub struct Run {
    #[arg(value_name = "PROJECT")]
    pub target: Option<PathBuf>,
    #[arg(
        short = 'd',
        long,
        help = "Start resident supervision and return after required startup readiness."
    )]
    pub detach: bool,
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
    pub suite_source: Option<String>,
}

#[derive(Debug)]
pub(crate) struct PreparedRun {
    pub(crate) ctx: PlanContext,
    pub(crate) plan: LaunchPlan,
    pub(crate) board: BoardBackend,
    pub(crate) specs: Vec<ParticipantSpec>,
    pub(crate) robot_targets: Vec<RobotFeedTarget>,
    /// The staged runtime layout root the plan's `bin/` binaries (including the
    /// infrastructure router) resolve from: `.phoxal/build/<triple>/` for a
    /// source run, the layout root itself for a staged/bundle run.
    pub(crate) staged_root: PathBuf,
    /// The router's resolved config file, if the compiled `robot.yaml` declares
    /// one.
    pub(crate) router_config: Option<PathBuf>,
}

/// Resources assembled after preparation but before the controller enters
/// supervision. Keeping this whole phase behind `drive_setup` means raw-mode
/// Ctrl-C remains polled until the supervisor loop takes ownership.
pub(crate) struct LiveRunSetup {
    pub(crate) router: InfrastructureRouter,
    pub(crate) board: BoardBackend,
    pub(crate) stages: Vec<SupervisionStage>,
    pub(crate) supervisor_options: SupervisorOptions,
    pub(crate) background_tasks: AbortTasks,
}

#[derive(Default)]
pub(crate) struct AbortTasks {
    handles: Vec<tokio::task::JoinHandle<()>>,
}

impl AbortTasks {
    pub(crate) fn push(&mut self, handle: tokio::task::JoinHandle<()>) {
        self.handles.push(handle);
    }

    pub(crate) fn extend(
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
            suite_source: app.suite_source.clone(),
        };
        if options.drivers == DriversMode::Off && !options.drivers_subset.is_empty() {
            bail!("--driver cannot be combined with --drivers off");
        }
        let target =
            crate::commands::resident::resolve_target(self.target.as_deref(), app.project.root())?;
        // SAFETY: command dispatch has not started worker threads for this run
        // yet; all project-local path helpers must agree on the selected root.
        unsafe {
            std::env::set_var(crate::host_paths::PROJECT_ROOT_ENV, &target.project);
        }
        if should_run_resident_in_process(
            app.output.interactive,
            self.detach,
            crate::resident::has_private_bootstrap(),
        ) {
            // `run` never owns the systemd notify socket; that is `start`'s job.
            return run_resident_supervision(app, target.project, options, None).await;
        }
        self.launch_client(app, target).await
    }

    async fn launch_client(
        &self,
        app: &AppContext,
        target: crate::commands::resident::ProjectTarget,
    ) -> Result<()> {
        let (mut launched, client) = connect_to_detached_resident(&target.project).await?;
        if self.detach {
            return wait_for_required_readiness(&client, &mut launched.child).await;
        }
        let result = crate::commands::resident::drive_tui(app, &target, client, true).await;
        if matches!(
            result,
            Ok(crate::session::controller::AttachmentOutcome::Terminal)
        ) {
            let status = tokio::task::spawn_blocking(move || launched.child.wait()).await??;
            anyhow::ensure!(status.success(), "resident supervisor exited with {status}");
        }
        result.map(|_| ())
    }
}

/// Spawn a detached resident supervisor, connect a client to it, and confirm the
/// running generation matches the one this launcher just bootstrapped. Shared by
/// `run` (interactive and `-d`) and `phoxal start` (interactive), which then
/// either drive the TUI or wait for required readiness and return.
pub(crate) async fn connect_to_detached_resident(
    project: &Path,
) -> Result<(
    crate::resident::LaunchedResident,
    phoxal_cli_client::SupervisorClient,
)> {
    let launched = crate::resident::launch_detached()?;
    let generation = match &launched.result {
        phoxal_cli_core::session::BootstrapResult::Bound {
            supervisor_generation,
            ..
        } => *supervisor_generation,
        phoxal_cli_core::session::BootstrapResult::Rejected { error } => {
            bail!("{error}; use `phoxal attach` or `phoxal stop` if another run owns the project")
        }
    };
    let socket = crate::resident::supervisor_socket_path(project)?;
    let client = phoxal_cli_client::SupervisorClient::connect(socket).await?;
    anyhow::ensure!(
        client.snapshots().current().supervisor_generation == generation,
        "resident generation did not match private bootstrap"
    );
    Ok((launched, client))
}

/// Drive a resident supervisor in this process: acquire the project lock, bind
/// the resident socket, prepare and supervise the graph, and return when it stops
/// or a shutdown signal arrives. `notify` is `Some` only for `phoxal start` under
/// systemd - the in-process foreground resident that owns `sd_notify`; `run` and
/// the detached-child `start` always pass `None`. A private-bootstrap failure is
/// reported back to the launcher so it never hangs waiting for the bootstrap
/// frame (#936).
pub(crate) async fn run_resident_supervision(
    app: &AppContext,
    project_root: PathBuf,
    options: RunOptions,
    notify: Option<crate::sd_notify::SdNotify>,
) -> Result<()> {
    run_resident_supervision_mode(app, project_root, ResidentMode::Run(options), notify).await
}

pub(crate) async fn run_webots_resident_supervision(
    app: &AppContext,
    project_root: PathBuf,
    options: crate::simulation::SimulateOptions,
) -> Result<()> {
    run_resident_supervision_mode(app, project_root, ResidentMode::Webots(options), None).await
}

enum ResidentMode {
    Run(RunOptions),
    Webots(crate::simulation::SimulateOptions),
}

async fn run_resident_supervision_mode(
    app: &AppContext,
    project_root: PathBuf,
    mode: ResidentMode,
    notify: Option<crate::sd_notify::SdNotify>,
) -> Result<()> {
    let nonce = crate::resident::private_bootstrap_nonce()?;
    match resident_supervision_inner(app, project_root, mode, nonce.clone(), notify).await {
        Ok(()) => Ok(()),
        Err(error) => {
            if nonce.is_some() {
                let _ = crate::resident::report_private_bootstrap(
                    &phoxal_cli_core::session::BootstrapResult::Rejected {
                        error: format!("{error:#}"),
                    },
                );
            }
            Err(error)
        }
    }
}

async fn resident_supervision_inner(
    app: &AppContext,
    project_root: PathBuf,
    mode: ResidentMode,
    launch_nonce: Option<phoxal_cli_core::session::LaunchNonce>,
    notify: Option<crate::sd_notify::SdNotify>,
) -> Result<()> {
    let identity = ProjectLockIdentity::resolve(&project_root, ProjectOperation::Run);
    let _lock = ProjectLock::acquire(identity)?;
    let execution_root = match &mode {
        ResidentMode::Run(_) => crate::runtime_paths::pin_installed_release(&project_root)?,
        ResidentMode::Webots(_) => project_root.clone(),
    };
    let board = BoardBackend::new();
    let execution = match &mode {
        ResidentMode::Run(_) => "run",
        ResidentMode::Webots(_) => "simulation:webots",
    };
    board.configure(project_root.display().to_string(), "resolving", execution);
    board.begin_phase("prepare");
    let token = tokio_util::sync::CancellationToken::new();
    let (action_tx, action_rx) = mpsc::channel(16);
    let socket = crate::resident::ResidentSocket::bind(
        &project_root,
        board.clone(),
        action_tx.clone(),
        token.clone(),
    )?;
    if let Some(launch_nonce) = launch_nonce {
        crate::resident::report_private_bootstrap(
            &phoxal_cli_core::session::BootstrapResult::Bound {
                supervisor_generation: board.supervisor_snapshot().supervisor_generation,
                launch_nonce,
            },
        )?;
    }

    let (events, events_rx) = mpsc::channel(16);
    let event_drain = tokio::spawn(drain_session_events(events_rx));
    let prepare_board = board.clone();
    let prepare_token = token.clone();
    let prepare_ui = app.ui;
    let prepare_output = app.output;
    let mut preparation = tokio::spawn(async move {
        match mode {
            ResidentMode::Run(options) => {
                let prepare_root = execution_root;
                let prepare_options = options.clone();
                let blocking_board = prepare_board.clone();
                let prepared = tokio::task::spawn_blocking(move || {
                    prepare_run(&prepare_root, prepare_options, &prepare_ui, blocking_board)
                })
                .await??;
                prepare_board.complete_phase("prepare");
                live_run_setup(
                    prepared,
                    prepare_ui,
                    prepare_output,
                    prepare_token,
                    events,
                    Some((action_tx, action_rx)),
                )
                .await
                .map(|setup| ResidentSetup {
                    router: setup.router,
                    board: setup.board,
                    stages: setup.stages,
                    supervisor_options: setup.supervisor_options,
                    background_tasks: setup.background_tasks,
                })
            }
            ResidentMode::Webots(options) => {
                let prepare_root = execution_root;
                let sim = tokio::task::spawn_blocking(move || {
                    crate::simulation::prepare(&prepare_root, options)
                })
                .await??;
                prepare_board.complete_phase("prepare");
                crate::simulation::live_simulate_setup(
                    prepare_ui,
                    sim,
                    prepare_board,
                    events,
                    prepare_token,
                    prepare_output,
                    Some((action_tx, action_rx)),
                )
                .await
                .map(|setup| ResidentSetup {
                    router: setup.router,
                    board: setup.board,
                    stages: setup.stages,
                    supervisor_options: setup.supervisor_options,
                    background_tasks: setup.background_tasks,
                })
            }
        }
    });
    let signal_token = token.clone();
    let signal_task = tokio::spawn(async move {
        if let Err(error) = resident_shutdown_signal().await {
            tracing::warn!("resident preparation signal watcher failed: {error:#}");
        }
        signal_token.cancel();
    });
    let prepared_result = tokio::select! {
        result = &mut preparation => Some(result?),
        () = token.cancelled() => None,
    };
    signal_task.abort();
    let Some(prepared_result) = prepared_result else {
        finish_cancelled_preparation(&board, &mut preparation, true).await;
        event_drain.abort();
        socket.close().await;
        return Ok(());
    };
    let prepared = match prepared_result {
        Ok(prepared) => prepared,
        Err(error) => {
            if token.is_cancelled() {
                board.set_lifecycle(phoxal_cli_core::session::ProjectLifecycle::Stopped);
                event_drain.abort();
                socket.close().await;
                return Ok(());
            }
            board.set_lifecycle(phoxal_cli_core::session::ProjectLifecycle::Failed);
            event_drain.abort();
            socket.close().await;
            return Err(error);
        }
    };
    let ResidentSetup {
        router,
        board,
        stages,
        supervisor_options,
        mut background_tasks,
    } = prepared;
    background_tasks.push(event_drain);
    // Under systemd the foreground resident owns readiness/watchdog signalling:
    // once the supervised graph reaches required readiness send `READY=1`, then
    // ping `WATCHDOG=1` on a timer. The task is a background task, so it is
    // aborted when supervision ends alongside the others.
    if let Some(notify) = notify {
        background_tasks.push(spawn_readiness_notify(notify, board.clone()));
    }
    let mut supervise = tokio::spawn(router.supervise(stages, board.clone(), supervisor_options));
    let outcome = tokio::select! {
        result = &mut supervise => result?,
        signal = resident_shutdown_signal() => {
            signal?;
            token.cancel();
            supervise.await?
        }
    };
    if outcome.is_err()
        && board.supervisor_snapshot().lifecycle
            != phoxal_cli_core::session::ProjectLifecycle::Failed
    {
        board.set_lifecycle(phoxal_cli_core::session::ProjectLifecycle::Failed);
    }
    drop(background_tasks);
    socket.close().await;
    outcome.map(|_| ())
}

async fn drain_session_events(
    mut events: mpsc::Receiver<phoxal_cli_core::session::event::SessionEvent>,
) {
    while events.recv().await.is_some() {}
}

async fn finish_cancelled_preparation<T>(
    board: &BoardBackend,
    preparation: &mut tokio::task::JoinHandle<T>,
    terminate_children: bool,
) {
    if terminate_children {
        crate::session::diagnostics::kill_active_children();
    }
    if tokio::time::timeout(PREPARATION_CANCEL_TIMEOUT, &mut *preparation)
        .await
        .is_err()
    {
        preparation.abort();
        let _ = preparation.await;
    }
    board.set_lifecycle(phoxal_cli_core::session::ProjectLifecycle::Stopped);
}

struct ResidentSetup {
    router: InfrastructureRouter,
    board: BoardBackend,
    stages: Vec<SupervisionStage>,
    supervisor_options: SupervisorOptions,
    background_tasks: AbortTasks,
}

/// Wait for the supervised graph to reach required readiness on the board, send
/// systemd `READY=1`, then send `WATCHDOG=1` at the notify socket's cadence until
/// the task is aborted. A graph that fails startup never signals ready - systemd
/// times the start out and marks the unit failed, which is the correct outcome.
fn spawn_readiness_notify(
    notify: crate::sd_notify::SdNotify,
    board: BoardBackend,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut snapshots = board.subscribe();
        loop {
            match required_readiness(&snapshots.borrow_and_update().clone()) {
                Readiness::Ready => break,
                Readiness::Failed(_) => return,
                Readiness::Pending => {}
            }
            if snapshots.changed().await.is_err() {
                return;
            }
        }
        if notify.notify_ready().is_err() {
            return;
        }
        let Some(interval) = notify.watchdog_interval() else {
            return;
        };
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            if notify.notify_watchdog().is_err() {
                return;
            }
        }
    })
}

/// Prepare a run without classifying "source versus compiled" beyond routing to
/// the right staging step (#936): a buildable source root refreshes staging and
/// runs it; a staged runtime layout runs in place; anything else is a precise
/// error. Both paths end at the same execution: the loader constructs the plan
/// from the staged layout.
fn prepare_run(
    root: &Path,
    options: RunOptions,
    ui: &crate::Ui,
    board: BoardBackend,
) -> Result<PreparedRun> {
    match classify_run_root(root)? {
        RunRoot::Source => prepare_run_on_board(root, options, ui, board),
        RunRoot::Layout => prepare_layout_run_on_board(root, options, board),
    }
}

#[derive(Debug)]
enum RunRoot {
    Source,
    Layout,
}

/// Classify a run root the way universal `run` does: a buildable source project
/// (a Cargo train anchor resolves) is staged and run; an already-staged runtime
/// layout (`robot.yaml` next to `bin/`, no source) runs in place; anything else
/// is a precise error. There is no implicit `/var/phoxal` fallback.
fn classify_run_root(root: &Path) -> Result<RunRoot> {
    if resolve_locked_train(root).is_ok() {
        return Ok(RunRoot::Source);
    }
    if RuntimeLayout::is_layout_root(root) {
        return Ok(RunRoot::Layout);
    }
    bail!(
        "{} is neither a buildable source project (no Cargo train anchor) nor a staged runtime layout (no robot.yaml next to bin/); run from a robot project or extract a build.phoxal bundle first",
        root.display()
    )
}

/// Keep private/headless residents in process; interactive foreground clients
/// launch a detached resident and attach to its socket.
const fn should_run_resident_in_process(
    interactive: bool,
    detach: bool,
    private_bootstrap: bool,
) -> bool {
    private_bootstrap || (!interactive && !detach)
}

async fn resident_shutdown_signal() -> Result<()> {
    let mut interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut hangup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())?;
    loop {
        tokio::select! {
            _ = interrupt.recv() => return Ok(()),
            _ = terminate.recv() => return Ok(()),
            _ = hangup.recv() => {
                tracing::debug!("ignored SIGHUP in resident supervisor");
            }
        }
    }
}

/// Whether the supervised graph has reached required readiness, from a board or
/// client snapshot. Shared by the detached-launcher wait (`run -d`, interactive
/// `start`) and the systemd readiness-notify task, so both apply the identical
/// rule: `Ready` is ready; `Degraded` is ready unless a startup-required process
/// has actually failed; `Failed` names the failed processes; everything else is
/// still pending.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Readiness {
    Ready,
    Pending,
    Failed(Vec<String>),
}

pub(crate) fn required_readiness(
    snapshot: &phoxal_cli_core::session::SupervisorSnapshotV0,
) -> Readiness {
    use phoxal_cli_core::session::{ProcessState, ProjectLifecycle, StartupRequirement};
    match snapshot.lifecycle {
        ProjectLifecycle::Ready => Readiness::Ready,
        ProjectLifecycle::Degraded => {
            let required_failed = snapshot.processes.values().any(|entry| {
                entry.descriptor.startup_requirement == StartupRequirement::Required
                    && entry.status.actual == ProcessState::Failed
            });
            if required_failed {
                Readiness::Pending
            } else {
                Readiness::Ready
            }
        }
        ProjectLifecycle::Failed => Readiness::Failed(
            snapshot
                .processes
                .iter()
                .filter(|(_, entry)| entry.status.actual == ProcessState::Failed)
                .map(|(key, _)| key.to_string())
                .collect(),
        ),
        _ => Readiness::Pending,
    }
}

pub(crate) async fn wait_for_required_readiness(
    client: &phoxal_cli_client::SupervisorClient,
    child: &mut std::process::Child,
) -> Result<()> {
    let mut snapshots = client.snapshots().subscribe();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5 * 60);
    loop {
        match required_readiness(&snapshots.borrow_and_update().clone()) {
            Readiness::Ready => return Ok(()),
            Readiness::Failed(failures) => {
                // A failure before any participant launched (e.g. plan
                // construction rejecting the layout) reaches Failed with an
                // empty process list; the resident logged the precise error.
                if failures.is_empty() {
                    bail!(
                        "resident startup failed before any participant launched; see the \
                         selected runtime state directory's supervisor.log for the exact error"
                    )
                }
                bail!("resident startup failed: {}", failures.join(", "))
            }
            Readiness::Pending => {}
        }
        if let Some(status) = child.try_wait()? {
            bail!("resident exited before readiness with {status}");
        }
        tokio::select! {
            result = snapshots.changed() => result.context("resident disconnected before readiness")?,
            _ = tokio::time::sleep(Duration::from_millis(100)) => {},
            _ = tokio::time::sleep_until(deadline) => bail!("timed out waiting for resident startup readiness"),
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn live_run_setup(
    mut prepared: PreparedRun,
    ui: crate::Ui,
    output: crate::session::output::OutputContext,
    token: tokio_util::sync::CancellationToken,
    events: mpsc::Sender<phoxal_cli_core::session::event::SessionEvent>,
    action_channel: Option<(
        mpsc::Sender<crate::supervisor::SupervisorAction>,
        mpsc::Receiver<crate::supervisor::SupervisorAction>,
    )>,
) -> Result<LiveRunSetup> {
    let (router, connect) = start_infrastructure_router(
        &prepared.staged_root,
        &prepared.ctx.project_root,
        prepared.router_config.clone(),
    )
    .await?;
    apply_session_connect(&mut prepared.plan, &mut prepared.specs, &connect);
    let revision =
        phoxal_cli_core::project::launch_plan::PlanRevision::compile(1, prepared.plan.clone())?;
    crate::supervisor::materialize_plan_binaries(
        &prepared.ctx.project_root,
        &revision,
        &mut prepared.specs,
    )?;
    prepared.board.set_router_status(format!("ready:{connect}"));
    prepared.board.set_state(
        phoxal_cli_core::session::ProcessKey::project("infrastructure-router"),
        crate::supervisor::ParticipantState::Ready,
        None,
    );
    ui.info(format!(
        "launch plan resolved: {} robot(s)",
        prepared.plan.robots.len()
    ));
    ui.info(format!("infrastructure router ready on {connect}"));
    report_launch_commands(&prepared.plan, &prepared.specs, &ui)?;

    let mut background_tasks = AbortTasks::default();
    background_tasks.extend(
        prepared
            .robot_targets
            .iter()
            .map(|target| {
                start_bus_log_subscriber(
                    target.scope.namespace.clone(),
                    target.scope.robot_id.clone(),
                    connect.clone(),
                    prepared.board.clone(),
                )
            })
            .collect::<Vec<_>>(),
    );
    background_tasks.extend(prepared.robot_targets.iter().map(|target| {
        start_liveliness_observer(
            target.scope.namespace.clone(),
            target.scope.robot_id.clone(),
            connect.clone(),
            prepared.board.clone(),
        )
    }));

    let (_action_tx, action_rx) = action_channel.unwrap_or_else(|| mpsc::channel(16));

    let stages = stages_for_run(prepared.specs, output);
    let starting = phoxal_cli_core::session::state::SessionState::Preparing
        .start()
        .expect("the controller begins every session in Preparing");
    let _ = events
        .send(phoxal_cli_core::session::event::SessionEvent::SessionChanged { state: starting })
        .await;

    let board = prepared.board;
    let supervisor_options = SupervisorOptions {
        action_rx: Some(crate::supervisor::SupervisorActionReceiver::new(action_rx)),
        token,
        events: Some(events),
        emits_running_on_startup_complete: true,
        ..SupervisorOptions::default()
    };

    Ok(LiveRunSetup {
        router,
        board,
        stages,
        supervisor_options,
        background_tasks,
    })
}

#[cfg(test)]
mod resident_role_tests {
    use super::should_run_resident_in_process;

    #[test]
    fn noninteractive_detach_still_uses_the_launcher() {
        assert!(!should_run_resident_in_process(false, true, false));
        assert!(should_run_resident_in_process(false, false, false));
        assert!(should_run_resident_in_process(false, true, true));
        assert!(!should_run_resident_in_process(true, true, false));
    }
}

/// The readiness decision `run -d`, interactive `start`, and the systemd
/// readiness-notify task all share (#936): it is what decides when to return
/// after a detached launch and when the foreground resident sends `READY=1`.
#[cfg(test)]
mod readiness_tests {
    use super::{Readiness, required_readiness};
    use crate::supervisor::{BoardBackend, ParticipantState};
    use phoxal_cli_core::session::{
        ParticipantKind, ParticipantStatus, ProcessKey, ProjectLifecycle, StartupRequirement,
    };

    #[test]
    fn readiness_tracks_lifecycle_and_required_failures() {
        let board = BoardBackend::new();
        board.configure("/tmp/project".to_string(), "v0.test", "run");
        // A freshly configured board is Starting - still pending.
        assert_eq!(
            required_readiness(&board.supervisor_snapshot()),
            Readiness::Pending
        );

        // Ready is ready.
        board.set_lifecycle(ProjectLifecycle::Ready);
        assert_eq!(
            required_readiness(&board.supervisor_snapshot()),
            Readiness::Ready
        );

        // Degraded with no required failure is ready enough to signal `READY=1`.
        board.set_lifecycle(ProjectLifecycle::Degraded);
        assert_eq!(
            required_readiness(&board.supervisor_snapshot()),
            Readiness::Ready
        );

        // A startup-required process that failed keeps a Degraded graph pending,
        // so readiness is never signalled on a broken required participant.
        let key = ProcessKey::project("drive");
        board.upsert_process(
            key.clone(),
            ParticipantStatus::new(
                "drive",
                ParticipantKind::Service,
                ParticipantState::Starting,
            ),
            StartupRequirement::Required,
        );
        board.set_state(&key, ParticipantState::Failed, Some("boom".to_string()));
        board.set_lifecycle(ProjectLifecycle::Degraded);
        assert_eq!(
            required_readiness(&board.supervisor_snapshot()),
            Readiness::Pending
        );

        // A failed lifecycle names the failed processes.
        assert!(matches!(
            required_readiness(&{
                board.set_lifecycle(ProjectLifecycle::Failed);
                board.supervisor_snapshot()
            }),
            Readiness::Failed(failures) if failures.iter().any(|failure| failure.contains("drive"))
        ));
    }
}

#[cfg(test)]
mod run_root_tests {
    use super::{RunRoot, classify_run_root};
    use std::fs;

    /// A staged runtime layout: `robot.yaml` next to a `bin/` store, no Cargo
    /// train anchor.
    fn write_layout(root: &std::path::Path) {
        fs::write(root.join("robot.yaml"), "schema: robot/v0\n").unwrap();
        fs::create_dir_all(root.join("bin")).unwrap();
    }

    #[test]
    fn classify_routes_layout_and_rejects_neither() {
        let layout = tempfile::tempdir().unwrap();
        write_layout(layout.path());
        assert!(matches!(
            classify_run_root(layout.path()).unwrap(),
            RunRoot::Layout
        ));

        // Neither a buildable source project nor a staged layout: a precise
        // error, no implicit fallback.
        let bare = tempfile::tempdir().unwrap();
        let error = classify_run_root(bare.path()).unwrap_err().to_string();
        assert!(
            error.contains("neither a buildable source project")
                && error.contains("staged runtime layout"),
            "{error}"
        );
    }
}

#[cfg(test)]
mod preparation_cancellation_tests {
    use super::{BoardBackend, drain_session_events, finish_cancelled_preparation};
    use phoxal_cli_core::session::ProjectLifecycle;
    use phoxal_cli_core::session::event::{DiagnosticLevel, DiagnosticSource, SessionEvent};

    #[tokio::test]
    async fn cancellation_during_preparation_finishes_stopped_not_failed() {
        let board = BoardBackend::new();
        board.set_lifecycle(ProjectLifecycle::Starting);
        let mut preparation = tokio::spawn(std::future::pending::<()>());
        // Other unit tests may be building fixtures concurrently in this
        // process. The production call passes `true`; this lifecycle-only test
        // avoids killing unrelated test-owned cargo process groups.
        finish_cancelled_preparation(&board, &mut preparation, false).await;
        assert_eq!(
            board.supervisor_snapshot().lifecycle,
            ProjectLifecycle::Stopped
        );
    }

    #[tokio::test]
    async fn unrendered_resident_events_are_continuously_drained() {
        let (tx, rx) = tokio::sync::mpsc::channel(16);
        let drain = tokio::spawn(drain_session_events(rx));
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            for index in 0..64 {
                tx.send(SessionEvent::Diagnostic {
                    source: DiagnosticSource::Cli,
                    level: DiagnosticLevel::Info,
                    message: index.to_string(),
                })
                .await
                .expect("drain stays open");
            }
        })
        .await
        .expect("event producer must not wedge after channel capacity");
        drop(tx);
        drain.await.expect("drain task");
    }
}
