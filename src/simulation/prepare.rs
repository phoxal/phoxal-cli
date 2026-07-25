//! Public plan-only preparation entry points.

use super::{
    SimPlan, SimulateOptions, build_checked_sim_launch_plan, resolve_project,
    sim_source_participants,
};
use anyhow::Result;
use phoxal_cli_core::project::launch_plan::PlanContext;
use std::path::Path;

pub(crate) fn prepare(project_start: &Path, options: SimulateOptions) -> Result<SimPlan> {
    let resolved = resolve_project(project_start, options.clone())?;
    let descriptors = phoxal_cli_core::artifacts::descriptors_for(&resolved.resolved, true, true)?;
    crate::native_artifacts::prepare_descriptors_with_preflight(&descriptors, None)?;
    let (plan, _contract_surfaces) = build_checked_sim_launch_plan(
        &resolved.project_root,
        &resolved.world_path,
        &resolved.resolved,
        resolved.suite.as_ref(),
    )?;
    let source_participants = sim_source_participants(
        &resolved.project_root,
        &resolved.resolved,
        resolved.suite.as_ref(),
    )?;
    Ok(SimPlan {
        plan,
        ctx: PlanContext {
            robot_path: resolved.robot_path,
            project_root: resolved.project_root,
            source: Some(phoxal_cli_core::project::launch_plan::PlanSource {
                resolved: resolved.resolved,
                source_participants,
            }),
        },
    })
}
