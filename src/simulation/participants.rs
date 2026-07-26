//! Simulation participant and substitution projections.

use crate::check::extract_participant_report_from_staged_runtime;
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
        let raw = extract_participant_report_from_staged_runtime(runtime).with_context(|| {
            format!(
                "failed to synthesize suite participant report for simulator {}",
                runtime.name
            )
        })?;
        if raw.artifact.kind != "simulator" || raw.artifact.id != runtime.name {
            bail!(
                "official simulator participant report artifact {} '{}' does not match expected simulator '{}'",
                raw.artifact.kind,
                raw.artifact.id,
                runtime.name
            );
        }
        let mut participant =
            graph_check::ParticipantApis::try_from(raw.clone()).with_context(|| {
                format!(
                    "failed to interpret participant report for simulator {}",
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

/// A simulation has exactly one simulator: the one participant that steps the
/// world.
///
/// The framework's now-deleted API-coherence pass used to enforce a stronger,
/// exact version of this rule: `ensure_exactly_one_timeline_authority`
/// counted which participant *published the world-clock contract*, over the
/// complete simulation contract-surface set (organization#957). That
/// per-contract inventory is gone along with the coherence pass; a simulator
/// count over the checked participant KIND is the honest static
/// approximation that remains expressible - any participant could in
/// principle mint a clock publisher, so this cannot fully replace the old
/// rule. The framework's own type system is what actually closes that gap
/// now (only a `#[phoxal::simulator]` participant can hold the
/// `WorldClockPublisher` capability, #952 section D); this check catches the
/// static, CLI-visible mistake the type system cannot see - a project
/// resolving/selecting the wrong number of simulator artifacts in the first
/// place (zero: nothing steps the world; more than one: two world histories
/// step the same participants).
///
/// Distinct from, and not superseded by,
/// `simulation::controllers::resolved_controller_runtime`: that function
/// counts `ResolvedPlatformRuntime` entries named `webots-controller` in the
/// RESOLVED runtime list, at STAGING time, purely to pick which one binary
/// `phoxal build`/live-simulate copies into the Webots controller layout. This
/// function runs later, over the CHECKED participant set (post
/// metadata-extraction, post simulator-id remapping) that the launch plan and
/// graph check consume, and counts by verified `ParticipantKind::Simulator`
/// role rather than by resolved-runtime name - the shape a locally-overridden
/// simulator would take if it never appears in `resolved.simulators` at all.
/// Neither check subsumes the other, so both stay.
pub(crate) fn ensure_exactly_one_simulator(
    participants: &[graph_check::ParticipantApis],
) -> Result<()> {
    let simulators = participants
        .iter()
        .filter(|participant| {
            participant.participant_kind == graph_check::ParticipantKind::Simulator
        })
        .map(|participant| participant.participant_id.as_str())
        .collect::<Vec<_>>();
    match simulators.len() {
        1 => Ok(()),
        0 => Err(anyhow!(
            "this simulation has no simulator: nothing would step the world, so no participant \
             would ever advance"
        )),
        _ => Err(anyhow!(
            "this simulation has more than one simulator ({}): the participants between them \
             would be stepped by two world histories at once",
            simulators.join(", ")
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn participant(id: &str, kind: graph_check::ParticipantKind) -> graph_check::ParticipantApis {
        graph_check::ParticipantApis {
            participant_id: id.to_string(),
            artifact_id: id.to_string(),
            participant_kind: kind,
            participant_class: graph_check::ParticipantClass::Checked,
            config_schema: None,
            scope: graph_check::ParticipantScope::Graph,
        }
    }

    #[test]
    fn exactly_one_simulator_passes() {
        let participants = [
            participant(
                "webots-controller-testbot",
                graph_check::ParticipantKind::Simulator,
            ),
            participant("drive", graph_check::ParticipantKind::Service),
        ];
        assert!(ensure_exactly_one_simulator(&participants).is_ok());
    }

    #[test]
    fn zero_simulators_is_rejected() {
        let participants = [participant("drive", graph_check::ParticipantKind::Service)];
        let error = ensure_exactly_one_simulator(&participants)
            .expect_err("a simulation with no simulator never steps");
        assert!(error.to_string().contains("no simulator"), "{error}");
    }

    #[test]
    fn more_than_one_simulator_is_rejected() {
        let participants = [
            participant(
                "webots-controller-testbot",
                graph_check::ParticipantKind::Simulator,
            ),
            participant(
                "second-controller-testbot",
                graph_check::ParticipantKind::Simulator,
            ),
        ];
        let error = ensure_exactly_one_simulator(&participants)
            .expect_err("two simulators are two world histories");
        let message = error.to_string();
        assert!(message.contains("more than one simulator"), "{message}");
        assert!(message.contains("webots-controller-testbot"), "{message}");
        assert!(message.contains("second-controller-testbot"), "{message}");
    }
}
