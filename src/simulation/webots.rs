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
        env: Vec::new(),
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
