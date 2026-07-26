//! Disposable Webots project generation and process preparation.

use super::{
    SimPlan, stage_simulation_for_robot, stage_simulator_controller_binaries, webots_world,
};
use crate::simulation::command::sim_source;
use crate::supervisor::ParticipantSpec;
use crate::webots_stage_root;
use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use phoxal_cli_core::session::ParticipantKind;
use phoxal_cli_core::session::WEBOTS_PROCESS_ID;
use phoxal_cli_core::session::{
    ProcessKey, ReadinessPolicy, RuntimeFailurePolicy, StartupRequirement,
};
use std::path::Path;

/// Stage the simulation world and build the Webots application process spec.
pub(crate) fn stage_and_prepare_webots_spec(
    ui: &crate::Ui,
    sim: &SimPlan,
    runtime_root: &Path,
    connect: &str,
    execution: phoxal::bus::ExecutionId,
) -> Result<ParticipantSpec> {
    crate::webots_stage_root::wipe_and_recreate()?;
    let world = webots_world(&sim.plan.mode);
    let staged = stage_simulation_for_robot(
        &sim.ctx.project_root,
        world,
        &sim_source(sim).resolved,
        &sim.plan,
        &[connect.to_string()],
        runtime_root,
    )?;
    stage_simulator_controller_binaries(&sim_source(sim).resolved, ui)?;
    let webots_path = crate::host_doctor::webots_executable_path()
        .map_err(|error| anyhow!("{error}"))
        .context("failed to locate the Webots executable for live simulate")?;
    ui.info(format!(
        "staged simulation to {}",
        webots_stage_root::root()?.display()
    ));
    ui.info(format!(
        "staged simulation world at {}",
        staged.staged_world_path.display()
    ));
    let spec = ParticipantSpec {
        key: ProcessKey::project(WEBOTS_PROCESS_ID),
        id: WEBOTS_PROCESS_ID.to_string(),
        kind: ParticipantKind::Tool,
        executable: webots_path,
        args: webots_launch_args(&staged.staged_world_path),
        cwd: None,
        // The run identity travels in the environment and nowhere else: not in
        // `controllerArgs`, not in the staged world text, and not in any file
        // inside the controller directory, so the controller directory stays a
        // run-invariant function of package content and the staged scene stays
        // a function of the robot model.
        env: webots_spawn_env(execution),
        shutdown_grace: std::time::Duration::from_secs(20),
        process_group: true,
        note: None,
        // The Webots application has no bus identity of its own, so readiness
        // is process-lifecycle-only.
        bus_participant: false,
        readiness: ReadinessPolicy::ProcessSpawned,
        startup_requirement: StartupRequirement::Required,
        runtime_failure: RuntimeFailurePolicy::StopProject,
        restart_policy: Default::default(),
    };
    Ok(spec)
}

/// The environment the Webots application is spawned with.
///
/// Exactly one variable, and it is deliberately the only one: the controller is
/// Webots' child, not ours, so this hop is the only way the supervised run
/// reaches it (#952 section B). Webots passes its environment through to the
/// controllers it spawns, so the controller joins the same execution root as
/// every service.
///
/// Nothing time-authoritative belongs here. The controller owns its own world
/// history and mints its own timeline; handing it an execution *origin* would
/// let it reconstruct real robot time it never reached.
fn webots_spawn_env(execution: phoxal::bus::ExecutionId) -> Vec<(String, String)> {
    vec![(
        phoxal::participant::launch::env::EXECUTION_ID.to_string(),
        execution.to_string(),
    )]
}

/// Build Webots' argv for a live simulate launch.
///
/// `--mode=realtime` is load-bearing, not cosmetic: Webots opens a world in the
/// PAUSED state by default, so without an explicit run mode the controller's
/// step callback is never called, `simulation/clock` never advances, and services
/// that use simulation time remain idle (session Liveliness can remain
/// present). `realtime` starts the simulation running,
/// synced to wall time so the operator watches the robot move at a natural
/// speed; the Webots controller owns logical time.
///
/// `--batch` suppresses Webots' blocking modal dialogs (notably the "save world
/// changes?" prompt on quit), so the CLI's requested SIGTERM stop can complete
/// without an operator having to dismiss a popup.
pub(crate) fn webots_launch_args(staged_world_path: &Path) -> Vec<String> {
    vec![
        "--mode=realtime".to_string(),
        "--batch".to_string(),
        staged_world_path.display().to_string(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use phoxal::participant::launch::env;

    /// The Webots hop carries the run and nothing else. A second variable here
    /// is not a detail: `PHOXAL_EXECUTION_ORIGIN` would hand the controller the
    /// real host clock's zero, which is exactly the robot time it must not be
    /// able to express.
    #[test]
    fn the_webots_process_receives_the_execution_and_nothing_time_authoritative() {
        let execution = phoxal::bus::ExecutionId::mint();
        let spawned = webots_spawn_env(execution);

        assert_eq!(
            spawned,
            vec![(env::EXECUTION_ID.to_string(), execution.to_string())]
        );
        for (key, _) in &spawned {
            assert_ne!(key, env::EXECUTION_ORIGIN, "the controller gets no clock");
            assert_ne!(key, env::PRODUCER_ID, "the controller mints its own");
        }
    }

    /// Two runs of the same project differ only in that environment: same
    /// argv, so the staged world path and Webots' own flags are run-invariant.
    #[test]
    fn two_runs_differ_only_in_the_environment_they_hand_webots() {
        let world = Path::new("/tmp/staged/worlds/rover.wbt");
        assert_eq!(webots_launch_args(world), webots_launch_args(world));

        let first = phoxal::bus::ExecutionId::mint();
        let second = phoxal::bus::ExecutionId::mint();
        assert_ne!(webots_spawn_env(first), webots_spawn_env(second));
        assert!(
            !webots_launch_args(world)
                .iter()
                .any(|arg| arg.contains(&first.to_string())),
            "the run identity must not enter Webots' argv"
        );
    }
}
