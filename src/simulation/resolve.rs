//! Project resolution and checked simulation launch-plan construction.

use super::{
    ResolvedSimulation, SimulateMode, SimulateOptions, driver_metadata_unavailable,
    official_simulator_participants, remap_simulator_participant_ids, remap_simulator_surface_ids,
    sim_checked_participants, sim_source_participants, simulated_component_records,
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
use phoxal_cli_core::project::catalog::Catalog;
use phoxal_cli_core::project::launch_plan::CheckedRobotLaunchInput;
use phoxal_cli_core::project::launch_plan::LaunchMode;
use phoxal_cli_core::project::launch_plan::LaunchPlan;
use phoxal_cli_core::project::launch_plan::STANDARD_SITE_TOOLS;
use phoxal_cli_core::project::launch_plan::build_launch_plan;
use phoxal_cli_core::project::launch_plan::simulator_controller_provider_id;
use phoxal_cli_core::project::resolver::ResolveOptions;
use phoxal_cli_core::project::resolver::ResolvedRobot;
use phoxal_cli_core::project::resolver::RobotManifestExtras;
use phoxal_cli_core::simulation::world;
use std::collections::BTreeMap;
use std::path::Path;

pub(crate) fn resolve_project(
    project_start: &Path,
    options: SimulateOptions,
    mode: SimulateMode,
) -> Result<ResolvedSimulation> {
    let robot_path = phoxal_cli_core::project::resolver::discover_robot_yaml(project_start)
        .with_context(|| format!("failed to find robot.yaml from {}", project_start.display()))?;
    let project_root = robot_path
        .parent()
        .context("robot.yaml did not have a parent directory")?
        .to_path_buf();
    let world_path = world::resolve_world(&project_root, &options.world)?;
    let loaded = if options.overlays.is_empty() {
        phoxal_cli_core::project::resolver::load_robot_with_extras(&robot_path)?
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

    // Always resolve live git component driver commits so driver metadata can
    // be staged. Component asset git refs are resolved only for live simulate,
    // where Webots world staging genuinely needs local asset files; dry-run
    // reports the intended staged paths without fetching assets.
    // The robot's own official artifacts (services + component drivers) resolve
    // for `--target` when set, so a Linux robot can be planned from a non-Linux
    // host; the simulator itself keeps the host target since Webots runs locally.
    let official_target = options
        .target
        .as_deref()
        .map(crate::resolver::resolve_target_triple)
        .transpose()?;
    let resolved = resolve(
        &robot,
        &project_root,
        catalog.as_ref(),
        ResolveOptions {
            resolve_source_commits: true,
            resolve_component_asset_commits: mode == SimulateMode::Live,
            official_target_triple: official_target,
            ..ResolveOptions::default()
        },
    )?;
    Ok(ResolvedSimulation {
        robot_path,
        project_root,
        world_path,
        resolved,
        manifest_extras,
        catalog,
    })
}

/// Build the checked simulation launch plan. Every source participant
/// (drivers, path-overridden services/simulators) rebuilds live - there is no
/// disk cache to scope a rebuild around (docs: `check::build_emit_apis_from_source`
/// never caches), so a `watch`-triggered recheck simply rebuilds the whole
/// source graph rather than just the one crate that changed.
/// Also returns the (already sim-filtered/remapped) contract surfaces
/// alongside the plan (finding A5) - the caller needs both to build a
/// `RuntimeStore`, and re-deriving them separately would duplicate the whole
/// metadata/check pass this function already ran.
pub(crate) fn build_checked_sim_launch_plan(
    project_root: &Path,
    world: &Path,
    resolved: &ResolvedRobot,
    manifest_extras: &RobotManifestExtras,
    catalog: Option<&Catalog>,
) -> Result<(LaunchPlan, Vec<graph_check::ParticipantContractSurface>)> {
    let source_participants = sim_source_participants(project_root, resolved, catalog)
        .with_context(|| "failed to prepare source participants for simulation metadata")?;
    // Finding A6: all three filters below admit exactly the standard site-
    // tool set (`STANDARD_SITE_TOOLS` - joypad/telemetry), derived
    // once in `launch_plan` and shared with `build_site_launches` there.
    // This used to hardcode only joypad, silently excluding
    // telemetry's declared graph contracts from validation even though
    // telemetry is started and readiness-waited exactly like the other two.
    let metadata_source_participants = source_participants
        .iter()
        .filter(|participant| {
            participant.kind != SourceParticipantKind::Tool
                || STANDARD_SITE_TOOLS.contains(&participant.name.as_str())
        })
        .cloned()
        .collect::<Vec<_>>();
    // A Catalog-sourced component driver is a platform ref here too (docs
    // #21), exactly like `check`/`run`/`deploy` - synthesized from catalog
    // metadata rather than built from source. Only a Path/Git-overridden
    // driver crate reaches the `build` closure below.
    let platform_refs = check_artifact_refs_from_resolved(resolved)
        .into_iter()
        .filter(|artifact| {
            artifact.kind != phoxal_cli_core::project::catalog::ArtifactKind::Tool
                || STANDARD_SITE_TOOLS.contains(&artifact.name.as_str())
        })
        .collect::<Vec<_>>();
    let tool_participants = tool_participants_from_resolved(resolved)?
        .into_iter()
        .filter(|tool| STANDARD_SITE_TOOLS.contains(&tool.name.as_str()))
        .collect::<Vec<_>>();
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
        CheckGraphContext { manifest_extras },
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
        |participant| {
            if participant.kind == SourceParticipantKind::ComponentDriver {
                return build_emit_apis_from_source(participant)
                    .map_err(|error| driver_metadata_unavailable(participant, error));
            }
            build_emit_apis_from_source(participant)
        },
    )?;

    let mut checked_participants = metadata_outcome.checked_participants.clone();
    let mut contract_surfaces = metadata_outcome.contract_surfaces.clone();
    remap_simulator_participant_ids(&mut checked_participants, &resolved.robot.robot.id)?;
    remap_simulator_surface_ids(&checked_participants, &mut contract_surfaces);
    let (official_simulators, official_simulator_surfaces) =
        official_simulator_participants(resolved)?;
    checked_participants.extend(official_simulators);
    contract_surfaces.extend(official_simulator_surfaces);
    let controller_provider_id = simulator_controller_provider_id(&resolved.robot.robot.id);
    let substitutions = simulated_component_records(&checked_participants, &controller_provider_id);
    let sim_participants = sim_checked_participants(&checked_participants);
    let sim_ids = sim_participants
        .iter()
        .map(|participant| participant.participant_id.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    contract_surfaces.retain(|surface| sim_ids.contains(surface.participant_id.as_str()));
    let report = graph_check::check_graph(&sim_participants);
    if !report.is_ok() {
        crate::check::ensure_check_outcome_ok(
            &resolved.channel.to_string(),
            &crate::check::CheckOutcome {
                missing_images: Vec::new(),
                report: report.clone(),
                checked_participants: sim_participants.clone(),
                contract_surfaces: Vec::new(),
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
            manifest_extras,
            checked_participants: &sim_participants,
            substitutions: &substitutions,
            source_participants: &source_participants,
        }],
    )?;
    let coherence_graph =
        crate::check::robot_contract_surfaces(&resolved.robot.robot.id, &contract_surfaces);
    let coherence = crate::check::coherence_for_launch_plan(&plan, &[coherence_graph])?;
    crate::check::enforce_coherence(crate::check::CoherenceVerb::Simulate, &coherence)?;
    Ok((plan, contract_surfaces))
}
