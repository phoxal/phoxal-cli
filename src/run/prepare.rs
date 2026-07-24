//! Prepare responsibilities for run.

use super::{DriverPolicy, PreparedRun, RunOptions, prepare_robot_participants};
use crate::check::CheckGraphContext;
use crate::check::build_emit_apis_from_source;
use crate::check::check_artifact_refs_from_resolved;
use crate::check::extract_emit_apis_from_staged_runtime;
use crate::check::extract_emit_apis_from_staged_tool;
use crate::check::fetch_emit_apis_from_tool;
use crate::check::run_check_with_context;
use crate::check::source_participants_from_resolved;
use crate::check::tool_participants_from_resolved;
use crate::component_driver::component_driver_crate_dir;
use crate::resolver::resolve;
use crate::supervisor::BoardBackend;
use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use phoxal_cli_core::project::launch_plan::CheckedRobotLaunchInput;
use phoxal_cli_core::project::launch_plan::LaunchMode;
use phoxal_cli_core::project::launch_plan::PlanContext;
use phoxal_cli_core::project::launch_plan::build_launch_plan;
use phoxal_cli_core::project::resolver::ResolveOptions;
use phoxal_cli_core::project::resolver::discover_robot_yaml;
use phoxal_cli_core::project::resolver::load_robot;
use phoxal_cli_core::session::{ProcessKey, StartupRequirement};
use std::collections::BTreeMap;
use std::path::Path;

pub(crate) fn prepare_run_on_board(
    project_start: &Path,
    options: RunOptions,
    ui: &crate::Ui,
    board: BoardBackend,
) -> Result<PreparedRun> {
    let robot_path = discover_robot_yaml(project_start)
        .with_context(|| format!("failed to find robot.yaml from {}", project_start.display()))?;
    let project_root = robot_path
        .parent()
        .context("robot.yaml did not have a parent directory")?;
    let robot = load_robot(&robot_path)?;
    let suite = crate::commands::load_suite_for_robot_from_source(
        options.suite_source.clone(),
        project_root,
    )?;
    let resolved = resolve(
        &robot,
        project_root,
        suite.as_ref(),
        ResolveOptions::default(),
    )?;
    let descriptors = phoxal_cli_core::artifacts::descriptors_for(&resolved, false, true)?;
    crate::native_artifacts::prepare_descriptors_with_preflight(&descriptors, Some(ui))?;
    let staged_root = crate::stager::stage_runtime_layout(project_root, &resolved)
        .context("failed to stage the runtime layout")?;

    let source_participants =
        source_participants_from_resolved(project_root, &resolved, component_driver_crate_dir)?;
    let checked_source_participants = source_participants.clone();
    let platform_refs = check_artifact_refs_from_resolved(&resolved);
    let tool_participants = tool_participants_from_resolved(&resolved)?;
    let mut official_by_ref = resolved
        .platform_runtimes
        .iter()
        .map(|runtime| (runtime.artifact_ref().to_string(), runtime))
        .collect::<BTreeMap<_, _>>();
    official_by_ref.extend(crate::check::component_driver_runtimes_by_ref(&resolved));
    let tools_by_ref = resolved
        .tools
        .iter()
        .map(|tool| (tool.asset.clone(), tool))
        .collect::<BTreeMap<_, _>>();
    let outcome = run_check_with_context(
        &platform_refs,
        &tool_participants,
        &checked_source_participants,
        CheckGraphContext {
            robot: Some(&robot),
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
        build_emit_apis_from_source,
    )?;
    if !outcome.is_ok() {
        crate::check::ensure_check_outcome_ok(&resolved.train, &outcome)?;
    }
    let plan = build_launch_plan(
        LaunchMode::Run,
        &[CheckedRobotLaunchInput {
            project_root,
            resolved: &resolved,
            checked_participants: &outcome.checked_participants,
            substitutions: &[],
            source_participants: &checked_source_participants,
        }],
    )?;
    let driver_policy = DriverPolicy::from_options(&options, &plan)?;
    let mut coherence_plan = plan.clone();
    for robot in &mut coherence_plan.robots {
        robot
            .participants
            .retain(|participant| driver_policy.launches(participant));
    }
    let coherence_graph =
        crate::check::robot_contract_surfaces(&resolved.robot.robot.id, &outcome.contract_surfaces);
    let coherence = crate::check::coherence_for_launch_plan(&coherence_plan, &[coherence_graph])?;
    crate::check::enforce_coherence(crate::check::CoherenceVerb::Run, &coherence)?;
    board.configure(
        project_root.display().to_string(),
        resolved.train.clone(),
        "run",
    );
    board.upsert_process(
        ProcessKey::project("infrastructure-router"),
        crate::supervisor::ParticipantStatus::new(
            "infrastructure-router",
            phoxal_cli_core::session::ParticipantKind::Tool,
            crate::supervisor::ParticipantState::Starting,
        ),
        StartupRequirement::Required,
    );
    let mut specs = Vec::new();

    prepare_robot_participants(
        &plan,
        &resolved,
        project_root,
        &driver_policy,
        &board,
        &mut specs,
        ui,
    )?;
    // Flatten every planned binary into the staged `bin/` under its canonical
    // identity name and repoint each spec at it, so execution consumes the
    // staged runtime layout (an identity-keyed lookup store) rather than the
    // cargo target dir / artifact store directly. The names are derived from the
    // plan participants and are exactly what the loader resolves against.
    let bin_names = crate::stager::canonical_bin_names(&plan);
    crate::stager::link_runtime_binaries(&staged_root, &mut specs, &bin_names)
        .context("failed to link planned binaries into the staged bin store")?;
    // Complete `bin/` into the loader's full required store: every dormant
    // catalog official plus the infrastructure router, none of which appears as
    // an active plan participant. `bin/` is then the true complete lookup store
    // an extracted bundle runs from with no source (#936/#945).
    crate::stager::stage_complete_official_store(&staged_root, &resolved, |crate_dir, name| {
        crate::run::build_source_binary(crate_dir, name, ui)
    })
    .context("failed to complete the staged bin store with the full official runtime set")?;

    let robot_targets = super::RobotFeedTarget::from_plan(&plan);
    let project_root = project_root.to_path_buf();
    let ctx = PlanContext {
        robot_path,
        project_root,
        resolved,
        source_participants,
    };

    Ok(PreparedRun {
        ctx,
        robot_targets,
        plan,
        board,
        specs,
    })
}
