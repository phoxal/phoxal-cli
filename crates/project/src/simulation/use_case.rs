use anyhow::{Context, Result};
use phoxal_cli_core::project::launch_plan::RunIdentity;
use phoxal_cli_core::runtime::ParticipantSpec;
use phoxal_cli_core::runtime::{
    ParticipantKind, ParticipantState, ProcessKey, ReadinessPolicy, RuntimeFailurePolicy,
    StartupRequirement, WEBOTS_PROCESS_ID,
};

use crate::{
    PrepareSimulationRequest, PreparedExecution, PreparedParticipant, PreparedRouter,
    PreparedSimulation,
};

pub(crate) fn prepare_simulation(request: PrepareSimulationRequest) -> Result<PreparedExecution> {
    let options = super::SimulateOptions {
        world: request.world,
        offline: request.offline,
    };
    crate::progress::ensure_active(request.reporter.as_ref())?;
    let resolved = super::resolve::resolve_project(
        &request.target.logical_root,
        options,
        request.reporter.as_ref(),
    )?;
    let source_participants =
        super::participants::sim_source_participants(&resolved.project_root, &resolved.resolved)?;
    let source_artifacts = {
        // WEBOTS_HOME is a build-time dependency of the source controller;
        // keep it out of metadata checking, staging, and spawned processes.
        let _webots_home = request
            .webots
            .home
            .as_deref()
            .map(super::webots::controller::WebotsHomeEnvGuard::set);
        crate::build::cargo::build_selected_source_artifacts(
            &source_participants,
            None,
            crate::build::profile::Profile::Debug,
            None,
            request.offline,
            request.reporter.as_ref(),
        )?
    };
    // A simulation bundle is `clock: simulated` with every driver block
    // stripped; the daemon never learns it is a simulation from anything else.
    let candidate = crate::stage::begin_runtime_layout(
        &resolved.project_root,
        &resolved.resolved,
        &phoxal_cli_core::project::intent::RunIntent::simulated(),
    )
    .context("failed to stage the simulation bundle")?;
    let mut plan = crate::progress::run_phase(
        request.reporter.as_ref(),
        crate::PhaseId::new("check"),
        "Checking simulation graph",
        || {
            super::resolve::build_checked_sim_launch_plan(
                super::resolve::CheckedSimulationInput {
                    project_root: &resolved.project_root,
                    world: &resolved.world_path,
                    resolved: &resolved.resolved,
                    candidate_root: candidate.path(),
                    source_participants: &source_participants,
                    source_artifacts: &source_artifacts,
                    offline: request.offline,
                    run: request.run,
                },
                request.reporter.as_ref(),
            )
        },
    )?;
    crate::progress::ensure_active(request.reporter.as_ref())?;
    let source_dirs = crate::run::participants::source_dirs_by_participant(&source_participants);
    let policy = crate::run::report::DriverPolicy::drivers_off_for_sim();
    let mut participants = crate::run::participants::prepare_robot_participants(
        &plan,
        &resolved.resolved,
        &source_dirs,
        &source_artifacts,
        candidate.path(),
        &policy,
    )?;
    crate::progress::ensure_active(request.reporter.as_ref())?;
    let candidate_path = candidate.path().to_path_buf();
    crate::progress::ensure_active(request.reporter.as_ref())?;
    let staged_root = crate::progress::run_phase(
        request.reporter.as_ref(),
        crate::PhaseId::new("publish"),
        "Publishing simulation runtime layout",
        || crate::stage::publish_runtime_layout(candidate),
    )?;
    for participant in &mut participants {
        if let Some(spec) = participant.launch.as_mut() {
            crate::run::prepare::repoint_after_publish(
                &mut spec.executable,
                &candidate_path,
                &staged_root,
            );
            if let Some(cwd) = spec.cwd.as_mut() {
                crate::run::prepare::repoint_after_publish(cwd, &candidate_path, &staged_root);
            }
        }
    }
    crate::run::prepare::repoint_plan_bundle_roots(&mut plan, &candidate_path, &staged_root);
    let router = PreparedRouter {
        config: crate::run::prepare::resolve_layout_router_config(&staged_root)?,
        endpoint: request.target.zenoh_endpoint.clone(),
    };
    crate::run::prepare::apply_session_connect(&mut plan, &mut participants, &router.endpoint);

    crate::progress::ensure_active(request.reporter.as_ref())?;
    super::webots::root::wipe_and_recreate(&resolved.project_root)?;
    let staged_world = super::webots::staging::stage_simulation_for_robot(
        &resolved.project_root,
        &resolved.world_path,
        &resolved.resolved,
        &plan,
        std::slice::from_ref(&router.endpoint),
        &staged_root,
    )?;
    crate::progress::ensure_active(request.reporter.as_ref())?;
    super::webots::controller::stage_simulator_controller_binaries(
        &resolved.project_root,
        &resolved.resolved,
        request.reporter.as_ref(),
        &source_artifacts,
        &staged_root,
    )?;
    let webots_key = ProcessKey::project(WEBOTS_PROCESS_ID);
    let webots_spec = ParticipantSpec {
        key: webots_key.clone(),
        id: WEBOTS_PROCESS_ID.to_string(),
        kind: ParticipantKind::Host,
        executable: request.webots.executable,
        args: webots_launch_args(&staged_world.staged_world_path),
        cwd: None,
        env: webots_environment(request.run),
        shutdown_grace: std::time::Duration::from_secs(20),
        process_group: true,
        note: None,
        bus_participant: false,
        readiness: ReadinessPolicy::ProcessSpawned,
        startup_requirement: StartupRequirement::Required,
        runtime_failure: RuntimeFailurePolicy::StopProject,
        restart_policy: Default::default(),
    };
    participants.push(PreparedParticipant {
        key: webots_key.clone(),
        id: WEBOTS_PROCESS_ID.to_string(),
        kind: ParticipantKind::Host,
        robot: None,
        local: true,
        startup_requirement: StartupRequirement::Required,
        initial_state: ParticipantState::Starting,
        note: None,
        launch: Some(webots_spec),
    });
    let webots_stage_root = super::webots::root::root(&resolved.project_root);
    Ok(PreparedExecution {
        target: request.target,
        project_root: resolved.project_root,
        staged_root,
        train: resolved.resolved.train.clone(),
        plan,
        participants,
        router,
        simulation: Some(PreparedSimulation {
            world: staged_world.staged_world_path,
            stage_root: webots_stage_root,
            stop_first: webots_key,
        }),
    })
}

/// Webots is a project-owned host process, not a bus participant and not a
/// time authority. It receives the execution identity needed by the controller
/// bridge, but never an execution origin or producer identity.
fn webots_environment(run: RunIdentity) -> Vec<(String, String)> {
    vec![(
        phoxal_runtime_contract::env::EXECUTION_ID.to_string(),
        run.execution().to_string(),
    )]
}

/// Build Webots' argv for a live simulation launch.
///
/// `--mode=realtime` is load-bearing: Webots opens a world paused by default,
/// so without an explicit run mode the controller never steps and the
/// simulation clock never advances. `--batch` suppresses blocking modal
/// dialogs so requested SIGTERM shutdown can complete unattended.
fn webots_launch_args(staged_world_path: &std::path::Path) -> Vec<String> {
    vec![
        "--mode=realtime".to_string(),
        "--batch".to_string(),
        staged_world_path.display().to_string(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn webots_environment_carries_only_the_execution_identity() {
        let run = RunIdentity::default();
        let env = webots_environment(run);
        assert_eq!(
            env,
            vec![(
                phoxal_runtime_contract::env::EXECUTION_ID.to_string(),
                run.execution().to_string()
            )]
        );
        assert!(
            env.iter()
                .all(|(key, _)| { key != phoxal_runtime_contract::env::EXECUTION_ORIGIN }),
            "Webots must not receive bus or time-authority identity"
        );
    }

    #[test]
    fn two_runs_change_only_the_environment_given_to_webots() {
        let world = std::path::Path::new("/tmp/staged/worlds/rover.wbt");
        assert_eq!(
            webots_launch_args(world),
            vec!["--mode=realtime", "--batch", "/tmp/staged/worlds/rover.wbt"]
        );
        let first = RunIdentity::default();
        let second = RunIdentity::default();
        assert_ne!(webots_environment(first), webots_environment(second));
        assert!(
            !webots_launch_args(world)
                .iter()
                .any(|arg| arg.contains(&first.execution().to_string()))
        );
    }
}
