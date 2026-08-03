//! Simulation participant and substitution projections.

use crate::validation::extract_participant_report_from_staged_runtime;
use crate::validation::source_participants_from_resolved_with_drivers;
use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use anyhow::bail;
use phoxal_cli_core::check as graph_check;
use phoxal_cli_core::check::source::SourceParticipant;
use phoxal_cli_core::project::launch_plan::SIMULATOR_CONTROLLER_ARTIFACT_NAME;
use phoxal_cli_core::project::launch_plan::simulator_controller_provider_id;
use phoxal_cli_core::project::resolver::BundlePlan;
use std::path::Path;

pub(crate) fn official_simulator_participants(
    prepared_root: &Path,
    resolved: &BundlePlan,
) -> Result<Vec<graph_check::ParticipantApis>> {
    let robot_id = resolved.source_manifest.robot.id.as_str();
    let simulation_bin_dir = prepared_root.join("bin");
    let mut participants = Vec::new();
    for runtime in resolved.simulators.iter().filter(|runtime| {
        runtime.source_path().is_none() && runtime.name == SIMULATOR_CONTROLLER_ARTIFACT_NAME
    }) {
        let raw = extract_participant_report_from_staged_runtime(&simulation_bin_dir, runtime)
            .with_context(|| {
                format!(
                    "failed to extract participant report for simulator {}",
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
/// `source_participants_from_resolved`, narrowed to what Webots actually
/// launches. Physical component drivers are substituted out in simulation,
/// so neither path/Git drivers nor registry drivers may enter its Cargo or
/// metadata preparation. The one selected simulator remains.
pub(crate) fn sim_source_participants(
    project_root: &Path,
    resolved: &BundlePlan,
) -> Result<Vec<SourceParticipant>> {
    let mut participants =
        source_participants_from_resolved_with_drivers(project_root, resolved, false)?;
    retain_simulation_sources(&mut participants);
    participants.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(participants)
}

fn retain_simulation_sources(participants: &mut Vec<SourceParticipant>) {
    participants.retain(|participant| {
        use phoxal_cli_core::check::source::SourceParticipantKind;
        match participant.kind {
            SourceParticipantKind::Simulator => {
                participant.expected_artifact_id == SIMULATOR_CONTROLLER_ARTIFACT_NAME
            }
            _ => true,
        }
    });
}

#[cfg(test)]
mod selection_tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn webots_source_selection_omits_physical_drivers_and_other_simulators() {
        let mut participants = vec![
            SourceParticipant::user_service("mission", PathBuf::from("runtimes/mission")),
            SourceParticipant::simulator(
                SIMULATOR_CONTROLLER_ARTIFACT_NAME,
                SIMULATOR_CONTROLLER_ARTIFACT_NAME,
                PathBuf::from("simulators/webots-controller"),
            ),
            SourceParticipant::simulator("other", "other", PathBuf::from("simulators/other")),
        ];

        retain_simulation_sources(&mut participants);
        assert_eq!(
            participants
                .iter()
                .map(|participant| participant.name.as_str())
                .collect::<Vec<_>>(),
            ["mission", SIMULATOR_CONTROLLER_ARTIFACT_NAME]
        );
    }
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
/// The framework's type system closes the accidental route to world-clock
/// publication: `simulation::Clock` does not satisfy the ordinary
/// state-publisher bound, and the documented builder is gated on the
/// `IsSimulator` marker. This CLI check catches the separate, statically
/// visible mistake of a project resolving or selecting the wrong number of
/// simulator artifacts in the first
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
