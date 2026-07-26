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
use phoxal_cli_core::project::launch_plan::SIMULATOR_CONTROLLER_ARTIFACT_NAME;
use phoxal_cli_core::project::launch_plan::simulator_controller_provider_id;
use phoxal_cli_core::project::resolver::ResolvedRobot;
use phoxal_cli_core::project::suite::Suite;
use std::path::Path;

pub(crate) fn official_simulator_participants(
    resolved: &ResolvedRobot,
) -> Result<Vec<graph_check::ParticipantApis>> {
    let robot_id = resolved.robot.robot.id.as_str();
    let mut participants = Vec::new();
    for runtime in resolved.simulators.iter().filter(|runtime| {
        runtime.source_path().is_none() && runtime.name == SIMULATOR_CONTROLLER_ARTIFACT_NAME
    }) {
        let raw = extract_emit_apis_from_staged_runtime(runtime).with_context(|| {
            format!(
                "failed to synthesize suite emit-apis for simulator {}",
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
                        "unrecognized simulator artifact name '{}'; expected '{}'",
                        runtime.name,
                        SIMULATOR_CONTROLLER_ARTIFACT_NAME
                    )
                },
            )?;
        participants.push(participant);
    }
    Ok(participants)
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
                        "unrecognized simulator artifact name '{}'; expected '{}'",
                        participant.artifact_id,
                        SIMULATOR_CONTROLLER_ARTIFACT_NAME
                    )
                })?;
    }
    Ok(())
}

/// Map the resolved Webots controller artifact to its compile-time graph id.
/// This identity never becomes a resident launch-plan or board row.
pub(crate) fn simulator_participant_id_for_resolved_artifact(
    artifact_name: &str,
    robot_id: &str,
) -> Option<String> {
    if artifact_name == SIMULATOR_CONTROLLER_ARTIFACT_NAME {
        Some(simulator_controller_provider_id(robot_id))
    } else {
        None
    }
}

/// Simulate's source participants: identical to
/// `source_participants_from_resolved` (a Suite-sourced driver is never a
/// source participant - see `component_driver_platform_refs_from_resolved`),
/// sorted for deterministic checking and staging. `suite` is accepted for
/// signature symmetry with the other `sim_*` helpers but no longer consulted.
pub(crate) fn sim_source_participants(
    project_root: &Path,
    resolved: &ResolvedRobot,
    _suite: Option<&Suite>,
) -> Result<Vec<SourceParticipant>> {
    let mut participants =
        source_participants_from_resolved(project_root, resolved, component_driver_crate_dir)?;
    participants.retain(|participant| {
        participant.kind != phoxal_cli_core::check::source::SourceParticipantKind::Simulator
            || participant.expected_artifact_id == SIMULATOR_CONTROLLER_ARTIFACT_NAME
    });
    participants.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(participants)
}

pub(crate) fn driver_metadata_unavailable(
    participant: &SourceParticipant,
    error: anyhow::Error,
) -> anyhow::Error {
    anyhow!(
        "DriverMetadataUnavailable: component driver crate '{}' for instance '{}' could not build on this host to extract its compiled-in API metadata section: {error:#}\n\nCustom and git-sourced driver crates must compile far enough on the dev host to emit the `#[derive(phoxal::Api)]` linker section; keep hardware transport behind a target cfg boundary such as `cfg(target_os = \"linux\")`. Alternatively use a verified artifact suite entry with inlined driver metadata.",
        participant.expected_artifact_id,
        participant.name
    )
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
