//! Live simulation supervision adapter.

use anyhow::{Context, Result};
use phoxal_cli_core::project::launch_plan::{PlanRevision, RunIdentity};
use tokio::sync::mpsc;

use crate::session::output::OutputContext;
use phoxal_cli_supervisor::{
    RequestedStop, SupervisionStage, SupervisorAction, SupervisorOptions, SupervisorState,
    start_liveliness_observer,
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

pub(crate) struct LiveSimSetup {
    pub(crate) router: phoxal_cli_supervisor::InfrastructureRouter,
    pub(crate) board: SupervisorState,
    pub(crate) stages: Vec<SupervisionStage>,
    pub(crate) supervisor_options: SupervisorOptions,
    pub(crate) background_tasks: crate::run::AbortTasks,
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn live_simulate_setup(
    ui: crate::Ui,
    prepared: phoxal_cli_project::PreparedExecution,
    board: SupervisorState,
    events: mpsc::Sender<phoxal_cli_core::session::event::SessionEvent>,
    token: tokio_util::sync::CancellationToken,
    output: OutputContext,
    action_channel: Option<(
        mpsc::Sender<SupervisorAction>,
        mpsc::Receiver<SupervisorAction>,
    )>,
    run: RunIdentity,
) -> Result<LiveSimSetup> {
    let simulation = prepared
        .simulation
        .as_ref()
        .context("project preparation did not return simulation data")?;
    board.configure(
        prepared.project_root.display().to_string(),
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
    let connect = prepared.router.endpoint.clone();
    let router = phoxal_cli_supervisor::start_infrastructure_router(
        prepared.router.binary.clone(),
        prepared.router.config.clone(),
        prepared.router.endpoint.clone(),
    )
    .await?;
    board.set_router_endpoint(connect.clone());
    board.set_state(
        phoxal_cli_core::session::ProcessKey::project("infrastructure-router"),
        phoxal_cli_core::session::ProcessState::Ready,
        None,
    );
    board.set_simulation_info("webots", simulation.world.display().to_string());
    ui.info(format!(
        "Webots profile: webots; world: {}; project: {}",
        simulation.world.display(),
        simulation.stage_root.display()
    ));

    let mut specs = prepared
        .participants
        .iter()
        .filter_map(|participant| participant.launch.clone())
        .collect::<Vec<_>>();
    let requested_spec = specs
        .iter()
        .find(|spec| spec.key == simulation.stop_first)
        .context("simulation stop-first participant has no launch spec")?;
    let requested_stop =
        RequestedStop::new(requested_spec.key.clone(), requested_spec.shutdown_grace);
    let revision = PlanRevision::compile(1, prepared.plan.clone())?;
    phoxal_cli_supervisor::materialize_plan_binaries(
        &prepared.project_root,
        &revision,
        &mut specs,
    )?;
    ui.info(format!(
        "simulation launch plan resolved: {} robot(s)",
        prepared.plan.robots.len()
    ));
    ui.info(format!("infrastructure router ready on {connect}"));
    crate::run::report_launch_commands(&prepared.plan, &specs, &ui)?;

    let mut background_tasks = crate::run::AbortTasks::default();
    background_tasks.extend(prepared.plan.robots.iter().map(|robot| {
        start_liveliness_observer(
            robot.namespace.clone(),
            robot.id.clone(),
            connect.clone(),
            run.execution(),
            board.clone(),
        )
    }));
    let (_action_tx, action_rx) = action_channel.unwrap_or_else(|| mpsc::channel(16));
    let stages = phoxal_cli_supervisor::stages_for_simulation(
        specs,
        output.wait_budget(super::SIMULATE_READINESS_TIMEOUT),
    );
    let starting = phoxal_cli_core::session::state::SessionState::Preparing
        .start()
        .expect("the controller begins every session in Preparing");
    let _ = events
        .send(phoxal_cli_core::session::event::SessionEvent::SessionChanged { state: starting })
        .await;
    Ok(LiveSimSetup {
        router,
        board,
        stages,
        supervisor_options: SupervisorOptions {
            action_rx: Some(phoxal_cli_supervisor::SupervisorActionReceiver::new(
                action_rx,
            )),
            requested_stop: Some(requested_stop),
            token,
            events: Some(events),
            emits_running_on_startup_complete: true,
        },
        background_tasks,
    })
}
