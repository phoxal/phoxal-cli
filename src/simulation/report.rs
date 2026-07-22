//! Human and structured simulation plan reporting.

use super::{
    SimPlan, WEBOTS_SITE_ID, native_tool_labels_from_plan,
    simulator_participant_id_for_resolved_artifact, substitution_lines,
};
use crate::webots_stage_root;
use anyhow::Result;
use phoxal_cli_core::project::launch_plan::DEFAULT_ROUTER_CONNECT;
use phoxal_cli_core::project::launch_plan::LaunchMode;
use phoxal_cli_core::project::launch_plan::LaunchPlan;
use phoxal_cli_core::project::resolver::ResolvedRobot;
use serde::Serialize;
use std::path::Path;
use std::path::PathBuf;

pub(crate) fn report_plan_only(sim: &SimPlan) -> Result<()> {
    let output = build_dry_run_output(sim);
    println!("framework train: {}", sim.ctx.resolved.train);
    println!(
        "official services ({}):",
        sim.ctx.resolved.platform_runtimes.len()
    );
    for runtime in &sim.ctx.resolved.platform_runtimes {
        println!("  - {} -> {}", runtime.name, runtime.artifact_ref());
    }
    println!("world: {}", output.world_path.display());
    println!("router: {}", output.bus_connect);
    // Out of the project tree now (`<project>/.phoxal/webots`,
    // see `webots_stage_root`), so print it explicitly for discoverability
    // even though nothing is written in dry-run mode.
    if let Ok(root) = webots_stage_root::root() {
        println!("staged simulation to {}", root.display());
    }
    println!("site tools:");
    for tool in &output.native_tools {
        println!("  - {tool}");
    }
    println!(
        "webots app (CLI-managed, id \"{WEBOTS_SITE_ID}\"): would launch pointed at staged world {}",
        output.webots_app.intended_staged_world_path.display()
    );
    if !output.simulator_artifacts.is_empty() {
        println!("simulator artifacts:");
        for artifact in &output.simulator_artifacts {
            println!("  - {artifact}");
        }
    }
    if !output.simulation_managed_participants.is_empty() {
        println!("simulation-managed participants (launched by Webots, not the CLI):");
        for participant in &output.simulation_managed_participants {
            println!("  - {participant}");
        }
    }
    if !output.substitutions.is_empty() {
        println!("substitutions:");
        for substitution in &output.substitutions {
            println!("  - {substitution}");
        }
    }
    println!("dry-run - no files written and no simulation processes started");
    Ok(())
}

/// Build the dry-run report body (Part 6): must show the Webots app as the
/// CLI-managed child, both simulator artifacts (supervisor + controller) with
/// their participant ids, and each simulator participant's SIMULATION-MANAGED
/// ownership + the intended staged world path. Never stages or launches
/// anything - the path is computed, not written.
pub(crate) fn build_dry_run_output(sim: &SimPlan) -> SimulateDryRunOutput {
    let substitutions = substitution_lines(&sim.plan);
    let simulator_artifacts = simulator_artifact_lines(&sim.ctx.resolved);
    let simulation_managed = simulation_managed_lines(&sim.plan);
    let world_path = webots_world(&sim.plan.mode).to_path_buf();
    let intended_staged_world_path = intended_staged_world_path(&world_path);
    let native_tools = native_tool_labels_from_plan(&sim.plan);
    SimulateDryRunOutput {
        mode: "dry-run",
        train: sim.ctx.resolved.train.clone(),
        world_path,
        bus_connect: DEFAULT_ROUTER_CONNECT.to_string(),
        platform_service_count: sim.ctx.resolved.platform_runtimes.len(),
        native_tools,
        substitutions,
        webots_app: WebotsAppSummary {
            site_id: WEBOTS_SITE_ID.to_string(),
            launch_ownership: "cli_managed".to_string(),
            intended_staged_world_path,
        },
        simulator_artifacts,
        simulation_managed_participants: simulation_managed,
    }
}

/// Extract the resolved `.wbt` world path a sim `LaunchPlan`'s mode carries.
/// `simulate` always builds `LaunchMode::Webots`, so any other mode here is a
/// caller bug, not a user-facing error.
pub(crate) fn webots_world(mode: &LaunchMode) -> &Path {
    match mode {
        LaunchMode::Webots { world } => world.as_path(),
        _ => unreachable!("simulate always builds a plan with LaunchMode::Webots"),
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct SimulateDryRunOutput {
    pub(crate) mode: &'static str,
    pub(crate) train: String,
    pub(crate) world_path: PathBuf,
    pub(crate) bus_connect: String,
    pub(crate) platform_service_count: usize,
    pub(crate) native_tools: Vec<String>,
    pub(crate) substitutions: Vec<String>,
    pub(crate) webots_app: WebotsAppSummary,
    pub(crate) simulator_artifacts: Vec<String>,
    pub(crate) simulation_managed_participants: Vec<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct WebotsAppSummary {
    pub(crate) site_id: String,
    pub(crate) launch_ownership: String,
    pub(crate) intended_staged_world_path: PathBuf,
}

/// The staged world path `simulate --dry-run` would produce, without actually
/// staging (Part 6: dry-run reports the intended path but never launches
/// Webots or writes staged files). Home-based (`webots_stage_root`), not
/// project-relative - see the module doc for why.
pub(crate) fn intended_staged_world_path(world_path: &Path) -> PathBuf {
    let world_name = world_path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("default");
    webots_stage_root::world_path(world_name).unwrap_or_else(|_| world_path.to_path_buf())
}

/// One line per resolved simulator artifact (supervisor + controller), naming
/// the artifact and its participant id.
pub(crate) fn simulator_artifact_lines(resolved: &ResolvedRobot) -> Vec<String> {
    let robot_id = resolved.robot.robot.id.as_str();
    resolved
        .simulators
        .iter()
        .filter_map(|runtime| {
            let participant_id =
                simulator_participant_id_for_resolved_artifact(&runtime.name, robot_id)?;
            Some(format!(
                "{} (artifact {}, participant id {participant_id})",
                runtime.name,
                runtime.artifact_ref()
            ))
        })
        .collect()
}

/// One line per SIMULATION-MANAGED participant in the plan: Webots (via the
/// supervisor) owns its lifecycle, not the CLI supervisor.
pub(crate) fn simulation_managed_lines(plan: &LaunchPlan) -> Vec<String> {
    plan.robots
        .iter()
        .flat_map(|robot| &robot.participants)
        .filter(|participant| {
            participant.launch_ownership
                == phoxal_cli_core::project::launch_plan::LaunchOwnership::SimulationManaged
        })
        .map(|participant| {
            format!(
                "{} (artifact {})",
                participant.launch.participant_id, participant.artifact_id
            )
        })
        .collect()
}

/// The bare participant ids of every SIMULATION-MANAGED participant in the
/// plan (the Webots supervisor plus one controller per robot) - the readiness
/// barrier's counterpart to `simulation_managed_lines`, which formats the same
/// filtered set for display instead of returning ids to wait on.
pub(crate) fn simulation_managed_participant_ids(plan: &LaunchPlan) -> Vec<String> {
    plan.robots
        .iter()
        .flat_map(|robot| &robot.participants)
        .filter(|participant| {
            participant.launch_ownership
                == phoxal_cli_core::project::launch_plan::LaunchOwnership::SimulationManaged
        })
        .map(|participant| participant.launch.participant_id.clone())
        .collect()
}
