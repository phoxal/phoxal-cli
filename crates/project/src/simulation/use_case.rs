use anyhow::{Context, Result};
use phoxal_bundle::RuntimeBundle;

use super::{PrepareSimulationRequest, PreparedSimulation, StageWebotsRequest, WebotsLaunch};

/// Resolve everything a simulation needs that a plain run does not: the world
/// to open, and the controller that drives it.
///
/// The bundle is deliberately not built here. A simulation runs the same
/// release `phoxal run` does - the same staging pass, the same bytes - so it
/// goes through the same launcher, and this only supplies the two host-side
/// pieces that launcher knows nothing about.
pub fn prepare_simulation(request: PrepareSimulationRequest) -> Result<PreparedSimulation> {
    crate::progress::ensure_active(request.reporter.as_ref())?;
    let project_root = crate::source::resolver::discover_robot_yaml(&request.target.logical_root)
        .with_context(|| {
            format!(
                "failed to find robot.yaml from {}",
                request.target.logical_root.display()
            )
        })?
        .parent()
        .context("robot.yaml did not have a parent directory")?
        .to_path_buf();
    // Resolve the world before anything is built: an unresolvable world name is
    // the operator's typo, and it should not cost a full staging pass to learn.
    let world_source = super::world::resolve_world(&project_root, &request.world)?;

    // WEBOTS_HOME is a build-time input of the controller's own crate. It is
    // scoped to this materialization and never reaches a spawned runtime.
    let controller = {
        let _webots_home = super::webots::controller::WebotsHomeEnvGuard::set(&request.webots.home);
        super::controller_tool::materialize(request.offline, request.reporter.as_ref())?
    };

    Ok(PreparedSimulation {
        project_root,
        world_source,
        webots_executable: request.webots.executable,
        controller,
    })
}

/// Stage the disposable Webots project: the controller, the meshes, the
/// generated PROTOs, and the world that names them.
pub fn stage_webots(request: StageWebotsRequest) -> Result<WebotsLaunch> {
    let bundle = RuntimeBundle::open(&request.bundle_root)
        .context("failed to read the bundle manifest the simulation runs")?;
    super::webots::root::wipe_and_recreate(&request.project_root)?;
    super::webots::controller::stage_controller(&request.project_root, &request.controller)?;
    let staged_world = super::webots::staging::stage_simulation_for_robot(
        &request.project_root,
        &request.world_source,
        &bundle,
        &request.endpoint,
    )?;
    Ok(WebotsLaunch {
        executable: request.webots_executable,
        args: webots_launch_args(&staged_world),
        world: staged_world,
    })
}

/// Build Webots' argv for a live simulation launch.
///
/// `--mode=realtime` is load-bearing: Webots opens a world paused by default,
/// so without an explicit run mode the controller never steps and the
/// simulation clock never advances. `--batch` suppresses blocking modal
/// dialogs so requested SIGTERM shutdown can complete unattended.
///
/// `--stdout`/`--stderr` are what make the controller's output reachable at
/// all. Webots owns the controller process and keeps its streams inside the
/// GUI console; these forward them to Webots' own, which this client has
/// already redirected into the session's Webots log. The controller is an
/// external entity that never publishes to the supervisor's log stream, so
/// without them its account of a failed simulation exists only in a console
/// nobody is watching.
fn webots_launch_args(staged_world_path: &std::path::Path) -> Vec<String> {
    vec![
        "--mode=realtime".to_string(),
        "--batch".to_string(),
        "--stdout".to_string(),
        "--stderr".to_string(),
        staged_world_path.display().to_string(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The controller's output has to leave Webots' own console, or a failed
    /// simulation has no readable account anywhere: the controller logs to
    /// stderr rather than to the supervisor's log stream.
    #[test]
    fn webots_host_arguments_are_stable_and_contain_no_runtime_identity() {
        let world = std::path::Path::new("/tmp/staged/worlds/rover.wbt");
        assert_eq!(
            webots_launch_args(world),
            vec![
                "--mode=realtime",
                "--batch",
                "--stdout",
                "--stderr",
                "/tmp/staged/worlds/rover.wbt"
            ]
        );
        assert!(
            !webots_launch_args(world)
                .iter()
                .any(|arg| arg == "--execution-id" || arg == "--participant-id")
        );
    }
}
