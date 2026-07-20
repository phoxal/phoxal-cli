//! Public plan-only preparation entry points.

use super::{
    SimPlan, SimulateMode, SimulateOptions, build_checked_sim_launch_plan, resolve_project,
    sim_source_participants,
};
use anyhow::Result;
use phoxal_cli_core::project::launch_plan::PlanContext;
use std::path::Path;

pub fn prepare(project_start: &Path, options: SimulateOptions) -> Result<SimPlan> {
    prepare_with_mode(project_start, options, SimulateMode::DryRun)
}

pub(crate) fn prepare_with_mode(
    project_start: &Path,
    options: SimulateOptions,
    mode: SimulateMode,
) -> Result<SimPlan> {
    let resolved = resolve_project(project_start, options.clone(), mode)?;
    if mode == SimulateMode::Live {
        let descriptors =
            phoxal_cli_core::artifacts::descriptors_for(&resolved.resolved, true, true)?;
        crate::native_artifacts::prepare_descriptors_with_preflight(&descriptors, None)?;
    }
    let (plan, contract_surfaces) = build_checked_sim_launch_plan(
        &resolved.project_root,
        &resolved.world_path,
        &resolved.resolved,
        &resolved.manifest_extras,
        resolved.catalog.as_ref(),
    )?;
    // Finding A5: resolved once here, from the same `plan`/`contract_surfaces`
    // this function already has - see `RuntimeStore::from_launch_plan`'s docs.
    let runtime_store = phoxal_cli_core::session::stores::runtime::RuntimeStore::from_launch_plan(
        &plan,
        &contract_surfaces,
    );
    let source_participants = sim_source_participants(
        &resolved.project_root,
        &resolved.resolved,
        resolved.catalog.as_ref(),
    )?;
    Ok(SimPlan {
        plan,
        ctx: PlanContext {
            robot_path: resolved.robot_path,
            project_root: resolved.project_root,
            resolved: resolved.resolved,
            source_participants,
        },
        runtime_store,
    })
}
