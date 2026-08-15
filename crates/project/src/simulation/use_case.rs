use anyhow::{Context, Result};
use phoxal_bundle::RuntimeBundle;
use phoxal_runtime_contract::metadata::ParticipantKind;

use super::{PrepareSimulationRequest, PreparedSimulation, StageWebotsRequest, WebotsLaunch};
use crate::run::PreparedExecution;

pub fn prepare_simulation(request: PrepareSimulationRequest) -> Result<PreparedExecution> {
    let options = super::SimulateOptions {
        world: request.world,
        offline: request.offline,
    };
    crate::progress::ensure_active(request.reporter.as_ref())?;
    let mut resolved = super::resolve::resolve_project(
        &request.target.logical_root,
        options,
        request.reporter.as_ref(),
    )?;
    resolved.resolved.source_manifest.clock = phoxal_manifest::source::robot::v0::Clock::Simulated;
    let mut canonical = serde_json::to_value(&resolved.resolved.compiled.robot)?;
    canonical["clock"] = serde_json::Value::String("simulated".to_string());
    resolved.resolved.compiled.robot = serde_json::from_value(canonical)
        .context("failed to derive the simulated canonical robot")?;
    // Refuse a cold/missing exact-train supervisor before compiling any
    // simulation participant. Participant builds remain development-profile;
    // the supervisor materializer is unconditionally release-profile.
    let target_dir =
        crate::build::cargo::cargo_target_dir(&resolved.project_root, request.offline)?;
    let supervisor = crate::build::materialise::materialize_supervisor(
        resolved.resolved.train.version(),
        None,
        request.offline,
        None,
        Some(target_dir),
        request.reporter.as_ref(),
    )?;
    crate::progress::ensure_active(request.reporter.as_ref())?;
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
    // stripped; the supervisor never learns it is a simulation from anything else.
    let candidate = crate::stage::begin_runtime_layout(&resolved.project_root, &resolved.resolved)
        .context("failed to stage the simulation bundle")?;
    crate::progress::run_phase(
        request.reporter.as_ref(),
        crate::progress_phase::PhaseId::new("check"),
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
                },
                request.reporter.as_ref(),
            )
        },
    )?;
    crate::progress::ensure_active(request.reporter.as_ref())?;
    crate::run::participants::stage_complete_bin_store(
        candidate.path(),
        &source_participants,
        &source_artifacts,
    )?;
    crate::stage::write_runtime_document(candidate.path(), &resolved.resolved)?;
    crate::progress::ensure_active(request.reporter.as_ref())?;
    let release = crate::progress::run_phase(
        request.reporter.as_ref(),
        crate::progress_phase::PhaseId::new("publish"),
        "Publishing the simulation deployment release",
        || {
            crate::stage::finalize_release(
                candidate,
                supervisor.path(),
                &crate::check::participant_metadata::expected_target_for_host(),
            )
        },
    )?;
    Ok(PreparedExecution {
        release,
        simulation: Some(PreparedSimulation {
            project_root: resolved.project_root,
            world_source: resolved.world_path,
            webots_executable: request.webots.executable,
        }),
    })
}

pub fn stage_webots(request: StageWebotsRequest) -> Result<WebotsLaunch> {
    let bundle = RuntimeBundle::open_verified(&request.staged_root)
        .context("failed to open the simulated runtime bundle")?;
    let simulators = bundle
        .participants()
        .iter()
        .filter(|participant| {
            bundle
                .artifacts()
                .get(participant.artifact())
                .is_some_and(|artifact| artifact.contract().kind == ParticipantKind::Simulator)
        })
        .collect::<Vec<_>>();
    anyhow::ensure!(
        simulators.len() == 1,
        "simulated runtime must contain exactly one simulator participant, found {}",
        simulators.len()
    );
    let simulator = simulators[0];
    let artifact = bundle
        .artifacts()
        .get(simulator.artifact())
        .context("simulator participant references no artifact")?;

    super::webots::root::wipe_and_recreate(&request.project_root)?;
    super::webots::controller::stage_bundled_controller(
        &request.project_root,
        &request.staged_root.join(artifact.path().as_str()),
    )?;
    let staged_world = super::webots::staging::stage_simulation_for_robot(
        &request.project_root,
        &request.world_source,
        &bundle,
        request.execution,
        simulator.id().clone(),
        &request.endpoint,
    )?;
    Ok(WebotsLaunch {
        executable: request.webots_executable,
        args: webots_launch_args(&staged_world.staged_world_path),
        cwd: None,
        world: staged_world.staged_world_path,
    })
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
    fn webots_host_arguments_are_stable_and_contain_no_runtime_identity() {
        let world = std::path::Path::new("/tmp/staged/worlds/rover.wbt");
        assert_eq!(
            webots_launch_args(world),
            vec!["--mode=realtime", "--batch", "/tmp/staged/worlds/rover.wbt"]
        );
        assert!(
            !webots_launch_args(world)
                .iter()
                .any(|arg| arg == "--execution-id" || arg == "--participant-id")
        );
    }
}
