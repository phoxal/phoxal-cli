//! Live-session orchestration for simulation.

use crate::cli::AppContext;
use anyhow::Context;
use anyhow::Result;
use std::path::Path;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct SimulateOptions {
    pub world: String,
    pub offline: bool,
}

pub(crate) async fn run_command(
    app: &AppContext,
    world: String,
    project: Option<&Path>,
    detach: bool,
) -> Result<()> {
    let target = crate::application::attachment::resolve_target(project, app.project.root())?;
    phoxal_cli_core::project::train::resolve_locked_train(&target.project, app.offline)
        .with_context(|| {
            format!(
                "simulation requires a buildable source project; {} is not a source project",
                target.project.display()
            )
        })?;
    // SAFETY: no task that reads the project-root environment has been spawned
    // on this simulation path.
    unsafe {
        std::env::set_var(phoxal_cli_project::PROJECT_ROOT_ENV, &target.project);
    }
    let options = SimulateOptions {
        world,
        offline: app.offline,
    };
    let resident_in_process = crate::application::run::should_run_resident_in_process(
        app.output.interactive,
        detach,
        phoxal_cli_supervisor::resident::has_private_bootstrap(),
    );
    if resident_in_process {
        return crate::application::run::run_webots_resident_supervision(
            app,
            target.project,
            options,
        )
        .await;
    }
    if detach {
        let (mut launched, feed, _) = crate::application::run::connect_to_detached_resident_feed(
            &target.project,
            app.offline,
        )
        .await?;
        return crate::application::run::wait_for_required_readiness(&feed, &mut launched.child)
            .await;
    }
    let (mut launched, feed, commands) =
        crate::application::run::connect_to_detached_resident(&target.project, app.offline).await?;
    let result = crate::application::attachment::run(app, &target, feed, commands, true).await;
    match result? {
        phoxal_cli_ui::AttachmentOutcome::ResidentFailed => {
            let _status = tokio::task::spawn_blocking(move || launched.child.wait()).await??;
            anyhow::bail!("resident supervisor failed")
        }
        phoxal_cli_ui::AttachmentOutcome::ResidentStopped => {
            let status = tokio::task::spawn_blocking(move || launched.child.wait()).await??;
            anyhow::ensure!(status.success(), "resident supervisor exited with {status}");
            Ok(())
        }
        phoxal_cli_ui::AttachmentOutcome::Detached => Ok(()),
    }
}

mod setup {

    use anyhow::{Context, Result};
    use phoxal_cli_core::project::launch_plan::{PlanRevision, RunIdentity};
    use tokio::sync::mpsc;

    use crate::cli::output::OutputContext;
    use phoxal_cli_supervisor::{
        RequestedStop, SupervisionStage, SupervisorAction, SupervisorOptions, SupervisorState,
        start_liveliness_observer,
    };

    pub(crate) struct LiveSimSetup {
        pub(crate) router: phoxal_cli_supervisor::InfrastructureRouter,
        pub(crate) board: SupervisorState,
        pub(crate) stages: Vec<SupervisionStage>,
        pub(crate) supervisor_options: SupervisorOptions,
        pub(crate) background_tasks: crate::application::run::AbortTasks,
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn live_simulate_setup(
        ui: crate::cli::Ui,
        prepared: phoxal_cli_project::PreparedExecution,
        board: SupervisorState,
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
            phoxal_cli_core::runtime::ProcessKey::project("infrastructure-router"),
            phoxal_cli_core::runtime::ParticipantKind::Tool,
            phoxal_cli_core::runtime::ProcessState::Starting,
            phoxal_cli_core::runtime::StartupRequirement::Required,
        );
        for participant in &prepared.participants {
            let state = crate::application::run::process_state(participant.initial_state);
            board.upsert_process(
                participant.key.clone(),
                participant.kind,
                state,
                participant.startup_requirement,
            );
            if participant.initial_state != phoxal_cli_core::runtime::ParticipantState::Starting
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
            phoxal_cli_core::runtime::ProcessKey::project("infrastructure-router"),
            phoxal_cli_core::runtime::ProcessState::Ready,
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
        crate::application::run::report_launch_commands(&prepared.plan, &specs, &ui)?;

        let mut background_tasks = crate::application::run::AbortTasks::default();
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
            output.wait_budget(super::super::SIMULATE_READINESS_TIMEOUT),
        );
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
                publishes_running_on_startup_complete: true,
            },
            background_tasks,
        })
    }
}

pub(crate) use setup::live_simulate_setup;
