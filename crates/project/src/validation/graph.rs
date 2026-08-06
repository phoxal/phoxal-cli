//! Graph responsibilities for check.

use super::{
    CheckGraphContext, CheckOutcome, PlatformArtifactRef, RawParticipantReport,
    ensure_brain_declares_unit_config, validate_artifact_identity,
    validate_source_artifact_identity, validate_user_service_config,
};
use anyhow::Context;
use anyhow::Result;
use phoxal_cli_core::check as graph_check;
use phoxal_cli_core::check::source::SourceParticipant;
use phoxal_cli_core::check::source::SourceParticipantKind;

pub fn run_check_with_context(
    resolved_platform_artifact_refs: &[PlatformArtifactRef],
    source_participants: &[SourceParticipant],
    context: CheckGraphContext<'_>,
    mut fetch: impl FnMut(&str) -> Result<RawParticipantReport>,
    mut build: impl FnMut(&SourceParticipant) -> Result<RawParticipantReport>,
) -> Result<CheckOutcome> {
    let mut participants = Vec::new();
    let mut config_problems = Vec::new();

    for artifact in resolved_platform_artifact_refs {
        let image_ref = &artifact.binary_name;
        let raw = fetch(image_ref).with_context(|| {
            format!(
                "failed to obtain participant report for {} {} ({image_ref})",
                artifact.kind_label(),
                artifact.name
            )
        })?;
        validate_artifact_identity(
            artifact.kind_label(),
            &artifact.name,
            artifact.kind.as_str(),
            &raw,
        )?;
        let participant =
            graph_check::ParticipantApis::try_from(raw.clone()).with_context(|| {
                format!(
                    "failed to interpret participant report for {} {} ({image_ref})",
                    artifact.kind_label(),
                    artifact.name
                )
            })?;
        if artifact.instances.is_empty() {
            participants.push(participant);
        } else {
            // A registry-materialized component driver is installed once but
            // launched once per instance that declares it - key each
            // instance's graph membership by its own id, exactly like a
            // workspace-built driver source participant does (see
            // `SourceParticipantKind::ComponentDriver` below).
            for instance in &artifact.instances {
                let mut instance_participant = participant.clone();
                instance_participant.participant_id = instance.clone();
                instance_participant.scope =
                    graph_check::ParticipantScope::ComponentInstance(instance.clone());
                participants.push(instance_participant);
            }
        }
    }

    for participant in source_participants {
        let raw = build(participant).with_context(|| {
            format!(
                "failed to obtain participant report for {} {} ({})",
                participant.kind_label(),
                participant.name,
                participant.crate_dir.display()
            )
        })?;
        validate_source_artifact_identity(participant, &raw)?;
        let mut participant_apis = graph_check::ParticipantApis::try_from(raw.clone())
            .with_context(|| {
                format!(
                    "failed to interpret participant report for {} {} ({})",
                    participant.kind_label(),
                    participant.name,
                    participant.crate_dir.display()
                )
            })?;
        if participant.kind == SourceParticipantKind::Brain {
            // The brain has no config side channel, so a non-unit schema is a
            // malformed binary rather than a user config mistake: fail hard
            // here, before any resident startup (organization#973).
            ensure_brain_declares_unit_config(participant, &raw)?;
        } else if participant.kind == SourceParticipantKind::ComponentDriver {
            // A component driver is launched once per component instance. Several
            // instances of the same driver share `artifact_id` (validated against
            // the emitted artifact identity), so key graph membership and
            // diagnostics by the concrete instance id instead.
            participant_apis.participant_id = participant.name.clone();
            participant_apis.scope =
                graph_check::ParticipantScope::ComponentInstance(participant.name.clone());
        } else if participant.kind == SourceParticipantKind::UserService
            && let Some(problem) = validate_user_service_config(
                &participant.name,
                participant_apis.config_schema.as_ref(),
                context.robot,
            )
        {
            config_problems.push(problem);
        }
        participants.push(participant_apis);
    }

    // Construct the shared report vehicle and append this pass's
    // `InvalidConfig` findings directly.
    let mut report = graph_check::Report::default();
    report.problems.extend(config_problems);
    Ok(CheckOutcome {
        report,
        checked_participants: participants,
    })
}
