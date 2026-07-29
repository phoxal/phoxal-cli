//! Command responsibilities for run.

use crate::AppContext;
use anyhow::bail;
use anyhow::{Context, Result};
use clap::Args;
use clap::ValueEnum;
use phoxal_cli_core::identity::ExecutionId;
use phoxal_cli_core::project::launch_plan::LaunchPlan;
use phoxal_cli_core::project::launch_plan::RunIdentity;
use phoxal_cli_supervisor::ParticipantSpec;
use phoxal_cli_supervisor::ProjectLock;
use phoxal_cli_supervisor::ProjectLockIdentity;
use phoxal_cli_supervisor::ProjectOperation;
use phoxal_cli_supervisor::SupervisionStage;
use phoxal_cli_supervisor::SupervisorOptions;
use phoxal_cli_supervisor::SupervisorState;
use phoxal_cli_supervisor::start_liveliness_observer;
use phoxal_cli_supervisor::{InfrastructureRouter, start_infrastructure_router};
use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RobotFeedTarget {
    pub(crate) scope: phoxal_cli_core::session::RobotScope,
}

impl RobotFeedTarget {
    fn from_plan(plan: &LaunchPlan) -> Vec<Self> {
        plan.robots
            .iter()
            .map(|robot| Self {
                scope: phoxal_cli_core::session::RobotScope {
                    namespace: robot.namespace.clone(),
                    robot_id: robot.id.clone(),
                },
            })
            .collect()
    }
}

const PREPARATION_CANCEL_TIMEOUT: Duration = if cfg!(test) {
    Duration::from_millis(25)
} else {
    Duration::from_secs(5)
};

fn process_state(
    state: phoxal_cli_core::session::ParticipantState,
) -> phoxal_cli_core::session::ProcessState {
    match state {
        phoxal_cli_core::session::ParticipantState::Starting => {
            phoxal_cli_core::session::ProcessState::Starting
        }
        phoxal_cli_core::session::ParticipantState::Ready => {
            phoxal_cli_core::session::ProcessState::Ready
        }
        phoxal_cli_core::session::ParticipantState::Degraded => {
            phoxal_cli_core::session::ProcessState::Degraded
        }
        phoxal_cli_core::session::ParticipantState::Failed => {
            phoxal_cli_core::session::ProcessState::Failed
        }
        phoxal_cli_core::session::ParticipantState::Restarting => {
            phoxal_cli_core::session::ProcessState::Restarting
        }
        phoxal_cli_core::session::ParticipantState::Stopped => {
            phoxal_cli_core::session::ProcessState::Stopped
        }
    }
}

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
    pub offline: bool,
}

#[derive(Debug)]
pub(crate) struct PreparedRun {
    pub(crate) project_root: PathBuf,
    pub(crate) plan: LaunchPlan,
    pub(crate) board: SupervisorState,
    pub(crate) specs: Vec<ParticipantSpec>,
    pub(crate) robot_targets: Vec<RobotFeedTarget>,
    pub(crate) router: phoxal_cli_project::PreparedRouter,
}

/// Resources assembled after preparation but before the controller enters
/// supervision. Keeping this whole phase behind `drive_setup` means raw-mode
/// Ctrl-C remains polled until the supervisor loop takes ownership.
pub(crate) struct LiveRunSetup {
    pub(crate) router: InfrastructureRouter,
    pub(crate) board: SupervisorState,
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
            offline: app.offline,
        };
        if options.drivers == DriversMode::Off && !options.drivers_subset.is_empty() {
            bail!("--driver cannot be combined with --drivers off");
        }
        let target =
            crate::commands::resident::resolve_target(self.target.as_deref(), app.project.root())?;
        // SAFETY: command dispatch has not started worker threads for this run
        // yet; all project-local path helpers must agree on the selected root.
        unsafe {
            std::env::set_var(phoxal_cli_project::PROJECT_ROOT_ENV, &target.project);
        }
        if should_run_resident_in_process(
            app.output.interactive,
            self.detach,
            phoxal_cli_supervisor::resident::has_private_bootstrap(),
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
        if self.detach {
            let (mut launched, feed, _) =
                connect_to_detached_resident_feed(&target.project).await?;
            return wait_for_required_readiness(&feed, &mut launched.child).await;
        }
        let (mut launched, feed, commands) = connect_to_detached_resident(&target.project).await?;
        let result = crate::application::attachment::run(app, &target, feed, commands, true).await;
        if matches!(
            result,
            Ok(phoxal_cli_ui::AttachmentOutcome::ResidentStopped)
        ) {
            let status = tokio::task::spawn_blocking(move || launched.child.wait()).await??;
            anyhow::ensure!(status.success(), "resident supervisor exited with {status}");
        }
        match result? {
            phoxal_cli_ui::AttachmentOutcome::ResidentFailed => {
                anyhow::bail!("resident supervisor failed")
            }
            phoxal_cli_ui::AttachmentOutcome::Detached
            | phoxal_cli_ui::AttachmentOutcome::ResidentStopped => Ok(()),
        }
    }
}

/// Spawn a detached resident supervisor, connect a client to it, and confirm the
/// running generation matches the one this launcher just bootstrapped. Shared by
/// `run` (interactive and `-d`) and `phoxal start` (interactive), which then
/// either drive the TUI or wait for required readiness and return.
pub(crate) async fn connect_to_detached_resident(
    project: &Path,
) -> Result<(
    phoxal_cli_supervisor::resident::LaunchedResident,
    phoxal_cli_client::SupervisorFeed,
    phoxal_cli_client::SupervisorCommands,
)> {
    let (launched, feed, socket) = connect_to_detached_resident_feed(project).await?;
    let commands = phoxal_cli_client::SupervisorCommands::connect(socket).await?;
    Ok((launched, feed, commands))
}

pub(crate) async fn connect_to_detached_resident_feed(
    project: &Path,
) -> Result<(
    phoxal_cli_supervisor::resident::LaunchedResident,
    phoxal_cli_client::SupervisorFeed,
    PathBuf,
)> {
    let launched = phoxal_cli_supervisor::resident::launch_detached()?;
    let generation = match &launched.result {
        phoxal_cli_protocol::BootstrapResult::Bound {
            supervisor_generation,
            ..
        } => *supervisor_generation,
        phoxal_cli_protocol::BootstrapResult::Rejected { error } => {
            bail!("{error}; use `phoxal attach` or `phoxal stop` if another run owns the project")
        }
    };
    let socket = phoxal_cli_supervisor::resident::supervisor_socket_path(project)?;
    let feed = phoxal_cli_client::SupervisorFeed::connect(socket.clone()).await?;
    anyhow::ensure!(
        feed.current().supervisor_generation == generation,
        "resident generation did not match private bootstrap"
    );
    Ok((launched, feed, socket))
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
    notify: Option<phoxal_cli_supervisor::systemd::notify::SdNotify>,
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
    notify: Option<phoxal_cli_supervisor::systemd::notify::SdNotify>,
) -> Result<()> {
    let execution = phoxal_cli_supervisor::resident::private_bootstrap_execution()?;
    match resident_supervision_inner(app, project_root, mode, execution, notify).await {
        Ok(()) => Ok(()),
        Err(error) => {
            if execution.is_some() {
                let _ = phoxal_cli_supervisor::resident::report_private_bootstrap(
                    &phoxal_cli_protocol::BootstrapResult::Rejected {
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
    bootstrap_execution: Option<ExecutionId>,
    notify: Option<phoxal_cli_supervisor::systemd::notify::SdNotify>,
) -> Result<()> {
    // One supervised run, one execution identity (#952 section B). A privately
    // launched resident adopts the one its launcher minted; a foreground run
    // mints its own. Recording it on the project lock is what lets an ad hoc
    // inspector join the running execution rather than an empty root of its own.
    let run =
        phoxal_cli_core::project::launch_plan::RunIdentity::mint_or_adopt(bootstrap_execution);
    let identity = ProjectLockIdentity::resolve(&project_root, ProjectOperation::Run)
        .in_execution(run.execution());
    let _lock = ProjectLock::acquire(identity)?;
    let runtime_target = phoxal_cli_project::resolve_target(Some(&project_root), &project_root)?;
    let board = SupervisorState::new();
    board.configure(
        project_root.display().to_string(),
        "resolving",
        run.execution(),
        runtime_target.zenoh_endpoint.clone(),
    );
    board.begin_phase("prepare");
    let token = tokio_util::sync::CancellationToken::new();
    let (action_tx, action_rx) = mpsc::channel(16);
    let socket = phoxal_cli_supervisor::resident::ResidentSocket::bind(
        &project_root,
        board.clone(),
        action_tx.clone(),
        token.clone(),
    )?;
    if bootstrap_execution.is_some() {
        phoxal_cli_supervisor::resident::report_private_bootstrap(
            &phoxal_cli_protocol::BootstrapResult::Bound {
                supervisor_generation: board.supervisor_snapshot().supervisor_generation,
                execution: run.execution(),
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
                let prepare_options = options.clone();
                let prepared = prepare_run(
                    runtime_target,
                    prepare_options,
                    prepare_ui,
                    prepare_board.clone(),
                    run,
                    prepare_token.clone(),
                )
                .await?;
                prepare_board.complete_phase("prepare");
                live_run_setup(
                    prepared,
                    prepare_ui,
                    prepare_output,
                    prepare_token,
                    events,
                    Some((action_tx, action_rx)),
                    run,
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
                crate::host_doctor::preflight()
                    .map_err(|error| anyhow::anyhow!("{error}"))
                    .context(
                        "Webots preflight failed; live simulate cannot launch the simulator",
                    )?;
                let executable = crate::host_doctor::webots_executable_path()
                    .map_err(|error| anyhow::anyhow!("{error}"))?;
                let home = crate::host_doctor::webots_home_path()
                    .map_err(|error| anyhow::anyhow!("{error}"))?;
                let offline = options.offline;
                let sim = phoxal_cli_project::prepare_simulation(
                    phoxal_cli_project::PrepareSimulationRequest {
                        target: runtime_target,
                        run,
                        world: options.world,
                        offline,
                        webots: phoxal_cli_project::WebotsHost {
                            executable,
                            home: Some(home),
                        },
                        reporter: Arc::new(crate::ui::PreparationReporter::new(
                            prepare_ui,
                            prepare_token.clone(),
                        )),
                    },
                )
                .await?;
                prepare_board.complete_phase("prepare");
                crate::simulation::live_simulate_setup(
                    prepare_ui,
                    sim,
                    prepare_board,
                    events,
                    prepare_token,
                    prepare_output,
                    Some((action_tx, action_rx)),
                    run,
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
        finish_cancelled_preparation(&board, &mut preparation).await;
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
    outcome
}

async fn drain_session_events(
    mut events: mpsc::Receiver<phoxal_cli_core::session::event::SessionEvent>,
) {
    while events.recv().await.is_some() {}
}

async fn finish_cancelled_preparation<T>(
    board: &SupervisorState,
    preparation: &mut tokio::task::JoinHandle<T>,
) {
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
    board: SupervisorState,
    stages: Vec<SupervisionStage>,
    supervisor_options: SupervisorOptions,
    background_tasks: AbortTasks,
}

/// Wait for the supervised graph to reach required readiness on the board, send
/// systemd `READY=1`, then send `WATCHDOG=1` at the notify socket's cadence until
/// the task is aborted. A graph that fails startup never signals ready - systemd
/// times the start out and marks the unit failed, which is the correct outcome.
fn spawn_readiness_notify(
    notify: phoxal_cli_supervisor::systemd::notify::SdNotify,
    board: SupervisorState,
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
async fn prepare_run(
    target: phoxal_cli_core::runtime::RuntimeTarget,
    options: RunOptions,
    ui: crate::Ui,
    board: SupervisorState,
    run: RunIdentity,
    cancellation: tokio_util::sync::CancellationToken,
) -> Result<PreparedRun> {
    let prepared = phoxal_cli_project::prepare_run(phoxal_cli_project::PrepareRunRequest {
        target,
        run,
        drivers: phoxal_cli_project::DriverRequest {
            mode: match options.drivers {
                DriversMode::On => phoxal_cli_project::DriverMode::On,
                DriversMode::Off => phoxal_cli_project::DriverMode::Off,
            },
            subset: options.drivers_subset,
        },
        offline: options.offline,
        reporter: Arc::new(crate::ui::PreparationReporter::new(ui, cancellation)),
    })
    .await?;
    board.configure(
        prepared.target.logical_root.display().to_string(),
        prepared.train.clone(),
        run.execution(),
        prepared.router.endpoint.clone(),
    );
    board.upsert_process(
        phoxal_cli_core::session::ProcessKey::project("infrastructure-router"),
        phoxal_cli_core::session::ParticipantKind::Tool,
        phoxal_cli_core::session::ProcessState::Starting,
        phoxal_cli_core::session::StartupRequirement::Required,
    );
    for participant in &prepared.participants {
        let state = process_state(participant.initial_state);
        board.upsert_process(
            participant.key.clone(),
            participant.kind,
            state,
            participant.startup_requirement,
        );
        if participant.initial_state != phoxal_cli_core::session::ParticipantState::Starting
            || participant.note.is_some()
        {
            board.set_state(participant.key.clone(), state, participant.note.clone());
        }
    }
    let specs = prepared
        .participants
        .iter()
        .filter_map(|participant| participant.launch.clone())
        .collect();
    Ok(PreparedRun {
        project_root: prepared.project_root,
        robot_targets: RobotFeedTarget::from_plan(&prepared.plan),
        plan: prepared.plan,
        board,
        specs,
        router: prepared.router,
    })
}

pub(crate) fn report_launch_commands(
    plan: &LaunchPlan,
    specs: &[ParticipantSpec],
    ui: &crate::Ui,
) -> Result<()> {
    let executions = plan
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
    ui.info("resolved launch participants:");
    for spec in specs {
        let kind = launch_kind_label(executions.get(spec.id.as_str()).copied());
        ui.info(format!(
            "  - {} ({kind}) -> {}",
            spec.id,
            spec.launch_command().command_line
        ));
    }
    ui.info(
        "motion guarantees: e-stop, source freshness, finite values, and robot-authored limits; autonomous motion also requires fresh typed safety constraints",
    );
    Ok(())
}

fn launch_kind_label(
    execution: Option<&phoxal_cli_core::project::launch_plan::ParticipantExecution>,
) -> &'static str {
    match execution {
        None => "robot-tool",
        Some(
            phoxal_cli_core::project::launch_plan::ParticipantExecution::OfficialArtifact {
                ..
            }
            | phoxal_cli_core::project::launch_plan::ParticipantExecution::OfficialTool { .. },
        ) => "official",
        Some(phoxal_cli_core::project::launch_plan::ParticipantExecution::UserService {
            ..
        }) => "user-service",
        Some(phoxal_cli_core::project::launch_plan::ParticipantExecution::UserTool { .. }) => {
            "user-tool"
        }
        Some(phoxal_cli_core::project::launch_plan::ParticipantExecution::ComponentDriver {
            ..
        }) => "driver",
    }
}

#[cfg(test)]
mod launch_kind_tests {
    use super::launch_kind_label;
    use phoxal_cli_core::project::launch_plan::ParticipantExecution;

    #[test]
    fn launch_kind_labels_cover_every_execution_variant_and_robot_tools() {
        let binary_name = || "fixture".to_string();
        assert_eq!(launch_kind_label(None), "robot-tool");
        assert_eq!(
            launch_kind_label(Some(&ParticipantExecution::OfficialArtifact {
                binary_name: binary_name()
            })),
            "official"
        );
        assert_eq!(
            launch_kind_label(Some(&ParticipantExecution::OfficialTool {
                binary_name: binary_name()
            })),
            "official"
        );
        assert_eq!(
            launch_kind_label(Some(&ParticipantExecution::UserService {
                binary_name: binary_name()
            })),
            "user-service"
        );
        assert_eq!(
            launch_kind_label(Some(&ParticipantExecution::UserTool {
                binary_name: binary_name()
            })),
            "user-tool"
        );
        assert_eq!(
            launch_kind_label(Some(&ParticipantExecution::ComponentDriver {
                binary_name: binary_name()
            })),
            "driver"
        );
    }
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
    snapshot: &phoxal_cli_protocol::SupervisorSnapshotV0,
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
    feed: &phoxal_cli_client::SupervisorFeed,
    child: &mut std::process::Child,
) -> Result<()> {
    let mut snapshots = feed.subscribe();
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
    output: crate::cli::output::OutputContext,
    token: tokio_util::sync::CancellationToken,
    events: mpsc::Sender<phoxal_cli_core::session::event::SessionEvent>,
    action_channel: Option<(
        mpsc::Sender<phoxal_cli_supervisor::SupervisorAction>,
        mpsc::Receiver<phoxal_cli_supervisor::SupervisorAction>,
    )>,
    run: RunIdentity,
) -> Result<LiveRunSetup> {
    let connect = prepared.router.endpoint.clone();
    let router = start_infrastructure_router(
        prepared.router.binary.clone(),
        prepared.router.config.clone(),
        prepared.router.endpoint.clone(),
    )
    .await?;
    let revision =
        phoxal_cli_core::project::launch_plan::PlanRevision::compile(1, prepared.plan.clone())?;
    phoxal_cli_supervisor::materialize_plan_binaries(
        &prepared.project_root,
        &revision,
        &mut prepared.specs,
    )?;
    prepared.board.set_router_endpoint(connect.clone());
    prepared.board.set_state(
        phoxal_cli_core::session::ProcessKey::project("infrastructure-router"),
        phoxal_cli_core::session::ProcessState::Ready,
        None,
    );
    ui.info(format!(
        "launch plan resolved: {} robot(s)",
        prepared.plan.robots.len()
    ));
    ui.info(format!("infrastructure router ready on {connect}"));
    report_launch_commands(&prepared.plan, &prepared.specs, &ui)?;

    let execution = run.execution();
    let mut background_tasks = AbortTasks::default();
    background_tasks.extend(prepared.robot_targets.iter().map(|target| {
        start_liveliness_observer(
            target.scope.namespace.clone(),
            target.scope.robot_id.clone(),
            connect.clone(),
            execution,
            prepared.board.clone(),
        )
    }));

    let (_action_tx, action_rx) = action_channel.unwrap_or_else(|| mpsc::channel(16));

    let stages = phoxal_cli_supervisor::stages_for_run(
        prepared.specs,
        output.wait_budget(super::RUN_STAGE_READY_TIMEOUT),
    );
    let starting = phoxal_cli_core::session::state::SessionState::Preparing
        .start()
        .expect("the controller begins every session in Preparing");
    let _ = events
        .send(phoxal_cli_core::session::event::SessionEvent::SessionChanged { state: starting })
        .await;

    let board = prepared.board;
    let supervisor_options = SupervisorOptions {
        action_rx: Some(phoxal_cli_supervisor::SupervisorActionReceiver::new(
            action_rx,
        )),
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
    use phoxal_cli_core::session::{
        ParticipantKind, ProcessKey, ProcessState, ProjectLifecycle, StartupRequirement,
    };
    use phoxal_cli_supervisor::SupervisorState;

    #[test]
    fn readiness_tracks_lifecycle_and_required_failures() {
        let board = SupervisorState::new();
        board.configure(
            "/tmp/project".to_string(),
            "v0.test",
            phoxal_cli_core::identity::ExecutionId::mint(),
            "unixsock-stream//tmp/project/.phoxal/zenoh.sock",
        );
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
            ParticipantKind::Service,
            ProcessState::Starting,
            StartupRequirement::Required,
        );
        board.set_state(&key, ProcessState::Failed, Some("boom".to_string()));
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
mod preparation_cancellation_tests {
    use super::{SupervisorState, drain_session_events, finish_cancelled_preparation};
    use phoxal_cli_core::session::ProjectLifecycle;
    use phoxal_cli_core::session::event::{DiagnosticLevel, DiagnosticSource, SessionEvent};

    #[tokio::test]
    async fn cancellation_during_preparation_finishes_stopped_not_failed() {
        let board = SupervisorState::new();
        board.set_lifecycle(ProjectLifecycle::Starting);
        let mut preparation = tokio::spawn(std::future::pending::<()>());
        // Other unit tests may be building fixtures concurrently in this
        // process. The production call passes `true`; this lifecycle-only test
        // avoids killing unrelated test-owned cargo process groups.
        finish_cancelled_preparation(&board, &mut preparation).await;
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
