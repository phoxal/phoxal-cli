//! Live simulation resource assembly and ownership.

use super::{SimPlan, stage_and_prepare_webots_spec, stages_for_simulate};
use crate::session::output::OutputContext;
use crate::simulation::command::sim_source;
use crate::supervisor::BoardBackend;
use crate::supervisor::RequestedStop;
use crate::supervisor::SupervisionStage;
use crate::supervisor::SupervisorAction;
use crate::supervisor::SupervisorOptions;
use crate::supervisor::start_bus_log_subscriber;
use crate::supervisor::start_liveliness_observer;
use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use anyhow::bail;
use tokio::sync::mpsc;

/// Everything [`live_simulate_setup`] hands back to the caller once it
/// completes: the resident supervisor inputs plus every ancillary task that
/// must be aborted once supervision ends.
pub(crate) struct LiveSimSetup {
    pub(crate) router: crate::run::InfrastructureRouter,
    pub(crate) board: BoardBackend,
    pub(crate) stages: Vec<SupervisionStage>,
    pub(crate) supervisor_options: SupervisorOptions,
    /// Every feed task that must stay alive for the whole session (log,
    /// Liveliness, and live telemetry observers) -
    /// collected here instead of leaked under `_`-prefixed bindings (finding
    /// B6), so the caller can abort every one of them once supervision ends.
    pub(crate) background_tasks: crate::run::AbortTasks,
}

/// Everything between preparation finishing and supervision beginning for a
/// `simulation webots run`: Webots preflight, ordinary resident staging,
/// disposable Webots project generation, and observer startup.
pub(crate) async fn live_simulate_setup(
    ui: crate::Ui,
    mut sim: SimPlan,
    board: BoardBackend,
    events: mpsc::Sender<phoxal_cli_core::session::event::SessionEvent>,
    token: tokio_util::sync::CancellationToken,
    output: OutputContext,
    action_channel: Option<(
        mpsc::Sender<SupervisorAction>,
        mpsc::Receiver<SupervisorAction>,
    )>,
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

    let staged_root =
        crate::stager::stage_runtime_layout(&sim.ctx.project_root, &sim_source(&sim).resolved)
            .context("failed to stage the simulation runtime layout")?;
    ensure_active()?;
    board.configure(
        sim.ctx.project_root.display().to_string(),
        sim_source(&sim).resolved.train.clone(),
        "simulation:webots",
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
    let mut specs = Vec::new();
    ensure_active()?;
    // Resolve the CLI-managed simulation participants (services and tools) from
    // the staged `bin/` under the same canonical identity names a native run
    // uses, so `simulation webots run` reads an identity-keyed `bin/` exactly
    // like `run`. The Webots-owned controller is copied into the Webots
    // project's controllers directory instead, so it is not resolved here.
    let source_dirs = crate::run::source_dirs_by_participant(&sim_source(&sim).source_participants);
    crate::run::prepare_robot_participants(
        &sim.plan,
        &sim_source(&sim).resolved,
        &source_dirs,
        &staged_root,
        &crate::run::DriverPolicy::drivers_off_for_sim(),
        &board,
        &mut specs,
        &ui,
    )?;
    // The router launches from the staged `bin/` entry like every other
    // official; stage it there before starting it.
    crate::stager::stage_router_binary(
        &staged_root,
        &sim_source(&sim).resolved,
        |crate_dir, name| crate::run::build_source_binary(crate_dir, name, &ui, None),
    )
    .context("failed to stage the infrastructure router into the simulation bin store")?;
    // Resolve the router config from the STAGED layout, matching `run`: staging
    // copied `router.config` into `staged_root` under its relative path (#936,
    // finding 4).
    let router_config =
        crate::run::resolve_router_config(&sim_source(&sim).resolved.robot, &staged_root)?;
    let (router, connect) =
        crate::run::start_infrastructure_router(&staged_root, &sim.ctx.project_root, router_config)
            .await?;
    board.set_router_status(format!("ready:{connect}"));
    board.set_state(
        phoxal_cli_core::session::ProcessKey::project("infrastructure-router"),
        crate::supervisor::ParticipantState::Ready,
        None,
    );
    crate::run::apply_session_connect(&mut sim.plan, &mut specs, &connect);
    ensure_active()?;
    let webots_spec = stage_and_prepare_webots_spec(&ui, &sim, &staged_root, &connect)?;
    let mut background_tasks = crate::run::AbortTasks::default();
    ui.info(format!(
        "Webots profile: webots; world: {}; project: {}",
        super::webots_world(&sim.plan.mode).display(),
        crate::webots_stage_root::root()?.display()
    ));
    board.set_simulation_info(
        "webots",
        super::webots_world(&sim.plan.mode).display().to_string(),
    );
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
    // OBSERVED readiness: drive ordinary resident participant state from each
    // participant's own Liveliness token.
    background_tasks.extend(sim.plan.robots.iter().map(|robot| {
        start_liveliness_observer(
            robot.namespace.clone(),
            robot.id.clone(),
            connect.clone(),
            board.clone(),
        )
    }));
    // The restart action channel is shared with ordinary resident supervision.
    let (_action_tx, action_rx) = action_channel.unwrap_or_else(|| mpsc::channel(16));

    let stages = stages_for_simulate(specs, output);

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

    Ok(LiveSimSetup {
        router,
        board,
        stages,
        supervisor_options,
        background_tasks,
    })
}
