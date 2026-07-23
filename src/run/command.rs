//! Command responsibilities for run.

use super::RobotFeedTarget;
use super::{
    InfrastructureRouter, apply_session_connect, prepare_run_on_board, report_launch_commands,
    stages_for_run, start_infrastructure_router,
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
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::Duration;
use tokio::sync::mpsc;

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
    pub suite_source: Option<String>,
    pub overlays: Vec<String>,
    pub watch: bool,
}

#[derive(Debug)]
pub(crate) struct PreparedRun {
    pub(crate) ctx: PlanContext,
    pub(crate) plan: LaunchPlan,
    pub(crate) board: BoardBackend,
    pub(crate) specs: Vec<ParticipantSpec>,
    pub(crate) robot_targets: Vec<RobotFeedTarget>,
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
            overlays: self.env.clone(),
            watch: self.watch,
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
            return self.run_resident(app, target.project, options).await;
        }
        self.launch_client(app, target, options).await
    }

    async fn launch_client(
        &self,
        app: &AppContext,
        target: crate::commands::resident::ProjectTarget,
        _options: RunOptions,
    ) -> Result<()> {
        let mut launched = crate::resident::launch_detached()?;
        let generation = match &launched.result {
            phoxal_cli_core::session::BootstrapResult::Bound {
                supervisor_generation,
                ..
            } => *supervisor_generation,
            phoxal_cli_core::session::BootstrapResult::Rejected { error } => {
                bail!(
                    "{error}; use `phoxal attach` or `phoxal stop` if another run owns the project"
                )
            }
        };
        let socket = crate::resident::supervisor_socket_path(&target.project)?;
        let client = phoxal_cli_client::SupervisorClient::connect(socket).await?;
        anyhow::ensure!(
            client.snapshots().current().supervisor_generation == generation,
            "resident generation did not match private bootstrap"
        );
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

    async fn run_resident(
        &self,
        app: &AppContext,
        project_root: PathBuf,
        options: RunOptions,
    ) -> Result<()> {
        let nonce = crate::resident::private_bootstrap_nonce()?;
        match self
            .run_resident_inner(app, project_root, options, nonce.clone())
            .await
        {
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

    async fn run_resident_inner(
        &self,
        app: &AppContext,
        project_root: PathBuf,
        options: RunOptions,
        launch_nonce: Option<phoxal_cli_core::session::LaunchNonce>,
    ) -> Result<()> {
        let identity = ProjectLockIdentity::resolve(&project_root, ProjectOperation::Run);
        let _lock = ProjectLock::acquire(identity)?;
        let board = BoardBackend::new();
        board.configure(project_root.display().to_string(), "resolving", "run");
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

        let prepare_root = project_root.clone();
        let ui = app.ui;
        let prepare_board = board.clone();
        let prepare_options = options.clone();
        let prepared = match tokio::task::spawn_blocking(move || {
            prepare_run_on_board(&prepare_root, prepare_options, &ui, prepare_board)
        })
        .await?
        {
            Ok(prepared) => prepared,
            Err(error) => {
                board.set_lifecycle(phoxal_cli_core::session::ProjectLifecycle::Failed);
                socket.close().await;
                return Err(error);
            }
        };
        board.complete_phase("prepare");
        let (events, _events_rx) = mpsc::channel(16);
        let setup = match live_run_setup(
            prepared,
            app.ui,
            options.watch,
            options,
            app.output,
            token.clone(),
            events,
            Some((action_tx, action_rx)),
        )
        .await
        {
            Ok(setup) => setup,
            Err(error) => {
                board.set_lifecycle(phoxal_cli_core::session::ProjectLifecycle::Failed);
                socket.close().await;
                return Err(error);
            }
        };
        let LiveRunSetup {
            router,
            board,
            stages,
            supervisor_options,
            background_tasks,
            ..
        } = setup;
        let mut supervise =
            tokio::spawn(router.supervise(stages, board.clone(), supervisor_options));
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
}

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

async fn wait_for_required_readiness(
    client: &phoxal_cli_client::SupervisorClient,
    child: &mut std::process::Child,
) -> Result<()> {
    use phoxal_cli_core::session::{ProcessState, ProjectLifecycle, StartupRequirement};
    let mut snapshots = client.snapshots().subscribe();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5 * 60);
    loop {
        let snapshot = snapshots.borrow_and_update().clone();
        match snapshot.lifecycle {
            ProjectLifecycle::Ready => return Ok(()),
            ProjectLifecycle::Degraded => {
                let required_failed = snapshot.processes.values().any(|entry| {
                    entry.descriptor.startup_requirement == StartupRequirement::Required
                        && entry.status.actual == ProcessState::Failed
                });
                if !required_failed {
                    return Ok(());
                }
            }
            ProjectLifecycle::Failed => {
                let failures = snapshot
                    .processes
                    .iter()
                    .filter(|(_, entry)| entry.status.actual == ProcessState::Failed)
                    .map(|(key, _)| key.to_string())
                    .collect::<Vec<_>>();
                bail!("resident startup failed: {}", failures.join(", "));
            }
            _ => {}
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
    watch_enabled: bool,
    watch_options: RunOptions,
    output: crate::session::output::OutputContext,
    token: tokio_util::sync::CancellationToken,
    events: mpsc::Sender<phoxal_cli_core::session::event::SessionEvent>,
    action_channel: Option<(
        mpsc::Sender<crate::supervisor::SupervisorAction>,
        mpsc::Receiver<crate::supervisor::SupervisorAction>,
    )>,
) -> Result<LiveRunSetup> {
    let (router, connect) =
        start_infrastructure_router(&prepared.ctx.resolved, &prepared.ctx.project_root, &ui)
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
        "launch plan resolved: {} robot(s), {} site tool(s)",
        prepared.plan.robots.len(),
        prepared.plan.site.len()
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

    let (action_tx, action_rx) = action_channel.unwrap_or_else(|| mpsc::channel(16));
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
