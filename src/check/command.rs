//! Command responsibilities for check.

use super::{
    CheckGraphContext, CheckOptions, CheckOutcome, RobotCoherenceDiagnostic,
    build_emit_apis_from_source_for_check, check_artifact_refs_from_resolved,
    component_driver_runtimes_by_ref, ensure_suite_availability, evaluate_robot_coherence,
    extract_emit_apis_from_staged_runtime, extract_emit_apis_from_staged_tool,
    fetch_emit_apis_from_tool, run_check_with_context, source_participants_from_resolved,
    tool_participants_from_resolved,
};
use crate::component_driver::component_driver_crate_dir;
use crate::resolver::resolve;
use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use phoxal_cli_core::project::resolver::ResolveOptions;
use phoxal_cli_core::project::resolver::discover_robot_yaml;
use phoxal_cli_core::project::resolver::load_robot;
use phoxal_cli_core::project::suite::ArtifactKind;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct CheckRunResult {
    pub(super) train: String,
    pub(super) participant_count: usize,
    pub(super) outcome: CheckOutcome,
    pub(super) coherence: Vec<RobotCoherenceDiagnostic>,
    pub(super) strict: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlatformArtifactRef {
    pub name: String,
    pub kind: ArtifactKind,
    pub artifact_ref: String,
    /// The component instance ids launching this artifact, for a
    /// `ComponentDriver` ref only. Empty for every other kind (a normal
    /// robot runtime participant). A suite-resolved component
    /// driver is fetched once but launched once per instance that declares
    /// it (`left_drive`/`right_drive` sharing one `phoxal/component-<id>
    /// -driver` package) - mirrors how [`SourceParticipant::component_driver_with_artifact_id`]
    /// keys a workspace-built driver's graph membership by instance, not
    /// by artifact id. Must not be empty when `kind == ComponentDriver`.
    pub instances: Vec<String>,
}

impl PlatformArtifactRef {
    pub(super) fn kind_label(&self) -> &'static str {
        match self.kind {
            ArtifactKind::Service => "official service",
            ArtifactKind::ComponentAssets => "official component assets",
            ArtifactKind::ComponentDriver => "official driver",
            ArtifactKind::Tool => "official tool",
            ArtifactKind::Simulator => "official simulator",
            ArtifactKind::Infrastructure => "official infrastructure",
        }
    }
}

pub(super) fn run(
    project_start: &std::path::Path,
    options: CheckOptions,
    ui: &crate::Ui,
) -> Result<CheckRunResult> {
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
    let target_triple = options
        .target
        .as_deref()
        .map(crate::resolver::resolve_target_triple)
        .transpose()?;
    let resolved = resolve(
        &robot,
        project_root,
        suite.as_ref(),
        ResolveOptions {
            official_target_triple: target_triple.clone(),
            tool_target_triple: target_triple,
            ..ResolveOptions::default()
        },
    )?;
    let descriptors = phoxal_cli_core::artifacts::descriptors_for(&resolved, false, false)?;
    crate::native_artifacts::prepare_descriptors_with_preflight(&descriptors, Some(ui))?;
    let platform_refs = check_artifact_refs_from_resolved(&resolved);
    ensure_suite_availability(&resolved)?;
    // Declaration drift (#950): name every workspace runtime crate robot.yaml
    // does not select, so authors catch a service or tool they forgot to
    // declare at check time.
    crate::run::report_undeclared_runtimes(&resolved.undeclared_runtimes, ui);
    let tool_participants = tool_participants_from_resolved(&resolved)?;
    let all_source_participants =
        source_participants_from_resolved(project_root, &resolved, component_driver_crate_dir)?;
    let source_participants = all_source_participants.as_slice();
    let platform_refs = platform_refs.as_slice();
    let participant_count =
        platform_refs.len() + tool_participants.len() + source_participants.len();
    let mut official_by_ref = resolved
        .platform_runtimes
        .iter()
        .map(|runtime| (runtime.artifact_ref().to_string(), runtime))
        .collect::<std::collections::BTreeMap<_, _>>();
    official_by_ref.extend(component_driver_runtimes_by_ref(&resolved));
    let tools_by_ref = resolved
        .tools
        .iter()
        .map(|tool| (tool.asset.clone(), tool))
        .collect::<std::collections::BTreeMap<_, _>>();
    let outcome = run_check_with_context(
        platform_refs,
        &tool_participants,
        source_participants,
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
        |participant| build_emit_apis_from_source_for_check(participant, ui),
    )?;

    let coherence = vec![evaluate_robot_coherence(
        &resolved.robot.robot.id,
        &outcome.contract_surfaces,
    )];
    Ok(CheckRunResult {
        train: resolved.train,
        participant_count,
        outcome,
        coherence,
        strict: options.strict,
    })
}
