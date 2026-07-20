//! Command responsibilities for run.

use super::{
    InfrastructureRouter, apply_session_connect, prepare_run, report_launch_commands,
    stages_for_run, start_infrastructure_router, start_telemetry_feeds_at,
};
use crate::AppContext;
use crate::supervisor::BoardBackend;
use crate::supervisor::ParticipantSpec;
use crate::supervisor::SupervisionStage;
use crate::supervisor::SupervisorIdentity;
use crate::supervisor::SupervisorLock;
use crate::supervisor::SupervisorOptions;
use crate::supervisor::start_bus_log_subscriber;
use crate::supervisor::start_liveliness_observer;
use anyhow::Result;
use anyhow::bail;
use clap::Args;
use clap::ValueEnum;
use phoxal_cli_core::project::launch_plan::LaunchPlan;
use phoxal_cli_core::project::launch_plan::PlanContext;
use std::collections::BTreeSet;
use std::time::Duration;
use tokio::sync::mpsc;

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
pub(crate) struct PreparedRun {
    pub(crate) ctx: PlanContext,
    pub(crate) plan: LaunchPlan,
    pub(crate) board: BoardBackend,
    pub(crate) specs: Vec<ParticipantSpec>,
    pub(crate) robot_log_targets: Vec<(String, String)>,
    /// Finding A5: this session's launch-time participant metadata, resolved
    /// once here from `plan` and the contract-check `outcome` - see
    /// `phoxal_cli_core::session::stores::runtime::RuntimeStore`'s own docs.
    pub(crate) runtime_store: phoxal_cli_core::session::stores::runtime::RuntimeStore,
}

/// Resources assembled after preparation but before the controller enters
/// supervision. Keeping this whole phase behind `drive_setup` means raw-mode
/// Ctrl-C remains polled until the supervisor loop takes ownership.
pub(crate) struct LiveRunSetup {
    router: InfrastructureRouter,
    connect: String,
    board: BoardBackend,
    telemetry: crate::telemetry::TelemetryBackend,
    runtime_store: phoxal_cli_core::session::stores::runtime::RuntimeStore,
    orderly_shutdown_timeout: Duration,
    stages: Vec<SupervisionStage>,
    supervisor_options: SupervisorOptions,
    background_tasks: AbortTasks,
    action_tx: mpsc::Sender<crate::supervisor::SupervisorAction>,
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
            catalog_source: app.catalog_source.clone(),
            overlays: self.env.clone(),
            watch: self.watch,
        };
        if options.drivers == DriversMode::Off && !options.drivers_subset.is_empty() {
            bail!("--driver cannot be combined with --drivers off");
        }
        let watch_enabled = options.watch;
        let watch_options = options.clone();

        let identity = SupervisorIdentity::resolve(
            app.project.root(),
            phoxal_cli_core::session::SessionMode::Run,
        );
        let _lock = SupervisorLock::acquire(identity)?;
        let project_root = app.project.root().to_path_buf();
        let ui = app.ui;

        // One interactive surface for the whole session (Product decision
        // 1): the controller starts the TUI's alternate screen right now,
        // before preparation
        // even begins - see `SessionController::new`'s docs.
        let mut controller = crate::session::controller::SessionController::new(
            app.output,
            phoxal_cli_core::session::SessionMode::Run,
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
    events: mpsc::Sender<phoxal_cli_core::session::event::SessionEvent>,
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
                start_liveliness_observer(
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
    let starting = phoxal_cli_core::session::state::SessionState::Preparing
        .start()
        .expect("the controller begins every session in Preparing");
    let _ = events
        .send(phoxal_cli_core::session::event::SessionEvent::SessionChanged { state: starting })
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
        action_rx: Some(crate::supervisor::SupervisorActionReceiver::new(action_rx)),
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
