//! Project resolution and checked simulation launch-plan construction.

use super::{
    ResolvedSimulation, SimulateOptions, driver_metadata_unavailable,
    official_simulator_participants, remap_simulator_participant_ids, sim_checked_participants,
    sim_source_participants,
};
use crate::check::CheckGraphContext;
use crate::check::build_emit_apis_from_source;
use crate::check::check_artifact_refs_from_resolved;
use crate::check::extract_emit_apis_from_staged_runtime;
use crate::check::extract_emit_apis_from_staged_tool;
use crate::check::fetch_emit_apis_from_tool;
use crate::check::run_check_with_context;
use crate::check::tool_participants_from_resolved;
use crate::resolver::resolve;
use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use phoxal::check as graph_check;
use phoxal_cli_core::check::source::SourceParticipantKind;
use phoxal_cli_core::project::launch_plan::CheckedRobotLaunchInput;
use phoxal_cli_core::project::launch_plan::LaunchMode;
use phoxal_cli_core::project::launch_plan::LaunchPlan;
use phoxal_cli_core::project::launch_plan::RunIdentity;
use phoxal_cli_core::project::launch_plan::build_launch_plan;
use phoxal_cli_core::project::resolver::ResolveOptions;
use phoxal_cli_core::project::resolver::ResolvedRobot;
use phoxal_cli_core::project::suite::Suite;
use phoxal_cli_core::simulation::world;
use std::collections::BTreeMap;
use std::path::Path;

pub(crate) fn resolve_project(
    project_start: &Path,
    options: SimulateOptions,
) -> Result<ResolvedSimulation> {
    let robot_path = phoxal_cli_core::project::resolver::discover_robot_yaml(project_start)
        .with_context(|| format!("failed to find robot.yaml from {}", project_start.display()))?;
    let project_root = robot_path
        .parent()
        .context("robot.yaml did not have a parent directory")?
        .to_path_buf();
    let world_path = world::resolve_world(&project_root, &options.world)?;
    let robot = phoxal_cli_core::project::resolver::load_robot(&robot_path)?;
    let suite = crate::commands::load_suite_for_robot_from_source(
        options.suite_source.clone(),
        &project_root,
    )?;

    // Resolve Cargo-workspace component drivers for compile-time metadata and
    // for their crate-owned model assets. Physical drivers are never launched.
    let resolved = resolve(
        &robot,
        &project_root,
        suite.as_ref(),
        ResolveOptions {
            ..ResolveOptions::default()
        },
    )?;
    Ok(ResolvedSimulation {
        robot_path,
        project_root,
        world_path,
        resolved,
        suite,
    })
}

/// Build the checked simulation launch plan. Every source participant
/// (drivers, path-overridden services/simulators) rebuilds live - there is no
/// disk cache for metadata extraction (`check::build_emit_apis_from_source`
/// never caches).
pub(crate) fn build_checked_sim_launch_plan(
    project_root: &Path,
    world: &Path,
    resolved: &ResolvedRobot,
    suite: Option<&Suite>,
    run: RunIdentity,
) -> Result<LaunchPlan> {
    let source_participants = sim_source_participants(project_root, resolved, suite)
        .with_context(|| "failed to prepare source participants for simulation metadata")?;
    let metadata_source_participants = source_participants.clone();
    // A Suite-sourced component driver is a platform ref here too (docs
    // #21), exactly like `check`/`run` - synthesized from suite
    // metadata rather than built from source. Only a Path/Git-overridden
    // driver crate reaches the `build` closure below.
    let platform_refs = check_artifact_refs_from_resolved(resolved);
    let tool_participants = tool_participants_from_resolved(resolved)?;
    let mut official_by_ref = resolved
        .platform_runtimes
        .iter()
        .map(|runtime| (runtime.artifact_ref().to_string(), runtime))
        .collect::<BTreeMap<_, _>>();
    official_by_ref.extend(crate::check::component_driver_runtimes_by_ref(resolved));
    let tools_by_ref = resolved
        .tools
        .iter()
        .map(|tool| (tool.asset.clone(), tool))
        .collect::<BTreeMap<_, _>>();

    let metadata_outcome = run_check_with_context(
        &platform_refs,
        &tool_participants,
        &metadata_source_participants,
        CheckGraphContext {
            robot: Some(&resolved.robot),
        },
        |artifact_ref| {
            if let Some(runtime) = official_by_ref.get(artifact_ref) {
                return extract_emit_apis_from_staged_runtime(runtime);
            }
            if let Some(tool) = tools_by_ref.get(artifact_ref) {
                return extract_emit_apis_from_staged_tool(tool);
            }
            Err(anyhow!(
                "resolved official artifact {artifact_ref} is not in the suite"
            ))
        },
        fetch_emit_apis_from_tool,
        |participant| {
            if participant.kind == SourceParticipantKind::ComponentDriver {
                return build_emit_apis_from_source(participant)
                    .map_err(|error| driver_metadata_unavailable(participant, error));
            }
            build_emit_apis_from_source(participant)
        },
    )?;

    let mut checked_participants = metadata_outcome.checked_participants.clone();
    remap_simulator_participant_ids(&mut checked_participants, &resolved.robot.robot.id)?;
    let official_simulators = official_simulator_participants(resolved)?;
    checked_participants.extend(official_simulators);
    let sim_participants = sim_checked_participants(&checked_participants);
    let report = graph_check::check_graph(&sim_participants);
    if !report.is_ok() {
        crate::check::ensure_check_outcome_ok(
            &resolved.train,
            &crate::check::CheckOutcome {
                missing_images: Vec::new(),
                report: report.clone(),
                checked_participants: sim_participants.clone(),
            },
        )?;
    }

    let plan = build_launch_plan(
        LaunchMode::Webots {
            world: world.to_path_buf(),
        },
        &[CheckedRobotLaunchInput {
            project_root,
            resolved,
            checked_participants: &sim_participants,
            substitutions: &[],
            source_participants: &source_participants,
        }],
        run,
    )?;
    Ok(plan)
}
