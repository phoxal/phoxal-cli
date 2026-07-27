//! Public plan-only preparation entry points.

use super::{
    SimPlan, SimulateOptions, build_checked_sim_launch_plan, resolve_project,
    sim_source_participants,
};
use anyhow::Result;
use phoxal_cli_core::project::launch_plan::{PlanContext, RunIdentity};
use std::path::Path;

pub(crate) fn prepare(
    project_start: &Path,
    options: SimulateOptions,
    run: RunIdentity,
) -> Result<SimPlan> {
    let offline = options.offline;
    let resolved = resolve_project(project_start, options)?;
    let plan = build_checked_sim_launch_plan(
        &resolved.project_root,
        &resolved.world_path,
        &resolved.resolved,
        offline,
        run,
    )?;
    let source_participants = sim_source_participants(&resolved.project_root, &resolved.resolved)?;
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
