//! Simulation participant and substitution projections.

use crate::check::extract_emit_apis_from_staged_runtime;
use crate::check::source_participants_from_resolved;
use crate::component_driver::component_driver_crate_dir;
use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use anyhow::bail;
use phoxal::check as graph_check;
use phoxal_cli_core::check::source::SourceParticipant;
use phoxal_cli_core::project::catalog::Catalog;
use phoxal_cli_core::project::launch_plan::SIMULATOR_CONTROLLER_ARTIFACT_NAME;
use phoxal_cli_core::project::launch_plan::SIMULATOR_SUPERVISOR_ARTIFACT_NAME;
use phoxal_cli_core::project::launch_plan::SIMULATOR_SUPERVISOR_PROVIDER_ID;
use phoxal_cli_core::project::launch_plan::SubstitutedContract;
use phoxal_cli_core::project::launch_plan::SubstitutionRecord;
use phoxal_cli_core::project::launch_plan::simulator_controller_provider_id;
use phoxal_cli_core::project::resolver::ResolvedRobot;
use std::collections::BTreeMap;
use std::path::Path;

pub(crate) fn official_simulator_participants(
    resolved: &ResolvedRobot,
) -> Result<(
    Vec<graph_check::ParticipantApis>,
    Vec<graph_check::ParticipantContractSurface>,
)> {
    let robot_id = resolved.robot.robot.id.as_str();
    let mut participants = Vec::new();
    let mut surfaces = Vec::new();
    for runtime in resolved
        .simulators
        .iter()
        .filter(|runtime| runtime.source_path().is_none())
    {
        let raw = extract_emit_apis_from_staged_runtime(runtime).with_context(|| {
            format!(
                "failed to synthesize catalog emit-apis for simulator {}",
                runtime.name
            )
        })?;
        if raw.artifact.kind != "simulator" || raw.artifact.id != runtime.name {
            bail!(
                "official simulator emit-apis artifact {} '{}' does not match expected simulator '{}'",
                raw.artifact.kind,
                raw.artifact.id,
                runtime.name
            );
        }
        let mut participant =
            graph_check::ParticipantApis::try_from(raw.clone()).with_context(|| {
                format!(
                    "failed to interpret emit-apis for simulator {}",
                    runtime.name
                )
            })?;
        participant.participant_id =
            simulator_participant_id_for_resolved_artifact(&runtime.name, robot_id).ok_or_else(
                || {
                    anyhow!(
                        "unrecognized simulator artifact name '{}'; expected '{}' or '{}'",
                        runtime.name,
                        SIMULATOR_SUPERVISOR_ARTIFACT_NAME,
                        SIMULATOR_CONTROLLER_ARTIFACT_NAME
                    )
                },
            )?;
        surfaces.push(crate::check::contract_surface(
            &raw,
            participant.participant_id.clone(),
        ));
        participants.push(participant);
    }
    Ok((participants, surfaces))
}

pub(crate) fn remap_simulator_surface_ids(
    participants: &[graph_check::ParticipantApis],
    surfaces: &mut [graph_check::ParticipantContractSurface],
) {
    let simulator_ids = participants
        .iter()
        .filter(|participant| {
            participant.participant_kind == graph_check::ParticipantKind::Simulator
        })
        .map(|participant| {
            (
                participant.artifact_id.as_str(),
                participant.participant_id.as_str(),
            )
        })
        .collect::<BTreeMap<_, _>>();
    for surface in surfaces {
        if let Some(participant_id) = simulator_ids.get(surface.participant_id.as_str()) {
            surface.participant_id = (*participant_id).to_string();
        }
    }
}

pub(crate) fn remap_simulator_participant_ids(
    participants: &mut [graph_check::ParticipantApis],
    robot_id: &str,
) -> Result<()> {
    for participant in participants.iter_mut().filter(|participant| {
        participant.participant_kind == graph_check::ParticipantKind::Simulator
    }) {
        participant.participant_id =
            simulator_participant_id_for_resolved_artifact(&participant.artifact_id, robot_id)
                .ok_or_else(|| {
                    anyhow!(
                        "unrecognized simulator artifact name '{}'; expected '{}' or '{}'",
                        participant.artifact_id,
                        SIMULATOR_SUPERVISOR_ARTIFACT_NAME,
                        SIMULATOR_CONTROLLER_ARTIFACT_NAME
                    )
                })?;
    }
    Ok(())
}

/// Map a resolved simulator artifact name (`ResolvedPlatformRuntime::name`,
/// e.g. `"webots-supervisor"` / `"webots-controller"`) to its participant id:
/// the supervisor gets the stable world-scoped id, the controller gets the
/// robot-scoped substitution-provider id. `None` for any other simulator
/// artifact name - callers decide whether that is a hard error (constructing
/// the participant) or simply "not one of the two known roles" (computing the
/// expected id set for parity checks).
pub(crate) fn simulator_participant_id_for_resolved_artifact(
    artifact_name: &str,
    robot_id: &str,
) -> Option<String> {
    if artifact_name == SIMULATOR_SUPERVISOR_ARTIFACT_NAME {
        Some(SIMULATOR_SUPERVISOR_PROVIDER_ID.to_string())
    } else if artifact_name == SIMULATOR_CONTROLLER_ARTIFACT_NAME {
        Some(simulator_controller_provider_id(robot_id))
    } else {
        None
    }
}

/// Simulate's source participants: identical to
/// `source_participants_from_resolved` (a Catalog-sourced driver is never a
/// source participant - see `component_driver_platform_refs_from_resolved`),
/// sorted for stable dry-run/watch-target output. `catalog` is accepted for
/// signature symmetry with the other `sim_*` helpers but no longer consulted
/// here now that catalog drivers route entirely through the platform-ref path.
pub(crate) fn sim_source_participants(
    project_root: &Path,
    resolved: &ResolvedRobot,
    _catalog: Option<&Catalog>,
) -> Result<Vec<SourceParticipant>> {
    let mut participants =
        source_participants_from_resolved(project_root, resolved, component_driver_crate_dir)?;
    participants.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(participants)
}

pub(crate) fn driver_metadata_unavailable(
    participant: &SourceParticipant,
    error: anyhow::Error,
) -> anyhow::Error {
    anyhow!(
        "DriverMetadataUnavailable: component driver crate '{}' for instance '{}' could not build on this host to extract its compiled-in API metadata section: {error:#}\n\nCustom and git-sourced driver crates must compile far enough on the dev host to emit the `#[derive(phoxal::Api)]` linker section; keep hardware transport behind a target cfg boundary such as `cfg(target_os = \"linux\")`. Alternatively use a verified artifact catalog entry with inlined driver metadata.",
        participant.expected_artifact_id,
        participant.name
    )
}

/// Board display only: which component-driver participants sim dropped from
/// the checked set (see `sim_checked_participants`) are instead "simulated by"
/// the Webots controller. This is not a checked fact - `phoxal::check` (0.28+)
/// no longer has a substitution concept, materializes nothing, and exposes no
/// way to recover a materialized topic from a report - it is purely the CLI's
/// own record of a caller-side plan choice, so it can render "component X
/// simulated by webots-controller" on the sim board and dry-run output.
///
/// A driver's own contract report carries only `family` per contract now (no
/// `schema_id`/`topic`/`direction`, D1), so there is nothing left to
/// materialize per instance here - the record just carries the family for
/// display.
pub(crate) fn simulated_component_records(
    participants: &[graph_check::ParticipantApis],
    provider_participant_id: &str,
) -> Vec<SubstitutionRecord> {
    let mut records = participants
        .iter()
        .filter(|participant| {
            participant.participant_class.is_checked()
                && participant.participant_kind == graph_check::ParticipantKind::Driver
        })
        .filter_map(|participant| match &participant.scope {
            graph_check::ParticipantScope::ComponentInstance(instance) => {
                Some(SubstitutionRecord {
                    component_instance: instance.clone(),
                    provider_participant_id: provider_participant_id.to_string(),
                    provider_artifact_id: SIMULATOR_CONTROLLER_ARTIFACT_NAME.to_string(),
                    provider_kind: "simulator".to_string(),
                    contracts: participant
                        .contracts
                        .iter()
                        .map(|contract| SubstitutedContract {
                            family: contract.family.clone(),
                        })
                        .collect(),
                })
            }
            graph_check::ParticipantScope::Graph => None,
        })
        .collect::<Vec<_>>();
    records.sort_by(|left, right| {
        left.component_instance
            .cmp(&right.component_instance)
            .then_with(|| {
                left.provider_participant_id
                    .cmp(&right.provider_participant_id)
            })
    });
    records
}

pub(crate) fn sim_checked_participants(
    participants: &[graph_check::ParticipantApis],
) -> Vec<graph_check::ParticipantApis> {
    participants
        .iter()
        .filter(|participant| participant.participant_kind != graph_check::ParticipantKind::Driver)
        .cloned()
        .collect()
}
