//! Command responsibilities for check.

use super::{
    CheckGraphContext, CheckOptions, CheckOutcome, RobotCoherenceDiagnostic,
    build_emit_apis_from_source_for_check, check_artifact_refs_from_resolved,
    component_driver_runtimes_by_ref, ensure_catalog_availability, ensure_user_service_exists,
    evaluate_robot_coherence, extract_emit_apis_from_staged_runtime,
    extract_emit_apis_from_staged_tool, fetch_emit_apis_from_tool, run_check_with_context,
    source_participants_from_resolved, tool_participants_from_resolved,
};
use crate::component_driver::component_driver_crate_dir;
use crate::resolver::resolve;
use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use phoxal_cli_core::project::catalog::ArtifactKind;
use phoxal_cli_core::project::resolver::ResolveOptions;
use phoxal_cli_core::project::resolver::discover_robot_yaml;
use phoxal_cli_core::project::resolver::load_robot_with_extras;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct CheckRunResult {
    pub(super) channel: String,
    pub(super) catalog_snapshot: Option<String>,
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
    /// graph-scoped singleton participant). A catalog-resolved component
    /// driver is fetched once but launched once per instance that declares
    /// it (`left_drive`/`right_drive` sharing one `phoxal/component-<id>
    /// -driver` package) - mirrors how [`SourceParticipant::component_driver_with_artifact_id`]
    /// keys a path/git-overridden driver's graph membership by instance, not
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
    let loaded = if options.overlays.is_empty() {
        load_robot_with_extras(&robot_path)?
    } else {
        phoxal_cli_core::project::resolver::load_robot_with_extras_and_overlays(
            &robot_path,
            &options.overlays,
        )?
    };
    let robot = loaded.robot;
    let manifest_extras = loaded.extras;
    let catalog = crate::commands::catalog_or_vendored(
        phoxal_cli_core::project::catalog::load_pinned_catalog(
            phoxal_cli_core::project::catalog::CatalogLoadOptions {
                cli_source: options.catalog_source.clone(),
                robot_source: manifest_extras.catalog_source.as_ref().map(|source| {
                    if source.is_absolute() {
                        source.clone()
                    } else {
                        project_root.join(source)
                    }
                }),
                offline: false,
            },
            phoxal_cli_core::project::catalog::selection_channel(robot.artifacts.channel),
        ),
    )?;
    // `check` resolves live git component refs so component drivers can be
    // located and staged. A path-only / official-only graph needs no component
    // network; a git component pinned to a commit SHA resolves offline; a
    // tag/branch ref is resolved live via `git ls-remote` with an actionable
    // error if the network is unavailable.
    let target_triple = options
        .target
        .as_deref()
        .map(crate::resolver::resolve_target_triple)
        .transpose()?;
    let resolved = resolve(
        &robot,
        project_root,
        catalog.as_ref(),
        ResolveOptions {
            refresh_channel_head: false,
            resolve_source_commits: true,
            resolve_component_asset_commits: false,
            official_target_triple: target_triple.clone(),
            tool_target_triple: target_triple,
        },
    )?;
    let descriptors = phoxal_cli_core::artifacts::descriptors_for(&resolved, false, false)?;
    crate::native_artifacts::prepare_descriptors_with_preflight(&descriptors, Some(ui))?;
    let platform_refs = check_artifact_refs_from_resolved(&resolved);
    ensure_catalog_availability(&resolved)?;
    let tool_participants = tool_participants_from_resolved(&resolved)?;
    let all_source_participants =
        source_participants_from_resolved(project_root, &resolved, component_driver_crate_dir)?;
    if let Some(service_name) = options.service.as_deref() {
        ensure_user_service_exists(&resolved, service_name)?;
    }
    // `--service <name>` used to scope the (expensive) build to just the
    // named service, reusing disk-cached `emit-apis` for every other source
    // participant. That disk cache is gone (docs: no cross-invocation
    // caching - every source participant is rebuilt live every run), so
    // every source participant always builds now; `--service` still narrows
    // which official platform refs are checked (below).
    let source_participants = all_source_participants.as_slice();
    let platform_refs = if options.service.is_some() {
        &[][..]
    } else {
        platform_refs.as_slice()
    };
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
            manifest_extras: &manifest_extras,
        },
        |artifact_ref| {
            if let Some(runtime) = official_by_ref.get(artifact_ref) {
                return extract_emit_apis_from_staged_runtime(runtime);
            }
            if let Some(tool) = tools_by_ref.get(artifact_ref) {
                return extract_emit_apis_from_staged_tool(tool);
            }
            Err(anyhow!(
                "resolved official artifact {artifact_ref} is not in the catalog"
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
        channel: resolved.channel.to_string(),
        catalog_snapshot: resolved.catalog_snapshot,
        participant_count,
        outcome,
        coherence,
        strict: options.strict,
    })
}
