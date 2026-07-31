//! Graph responsibilities for check.

use super::{
    CheckGraphContext, CheckOutcome, PlatformArtifactRef, RawParticipantReport,
    validate_artifact_identity, validate_source_artifact_identity, validate_user_service_config,
};
use anyhow::Context;
use anyhow::Result;
use phoxal_cli_core::check as graph_check;
use phoxal_cli_core::check::source::SourceParticipant;
use phoxal_cli_core::check::source::SourceParticipantKind;
use phoxal_cli_core::check::source::ToolParticipant;
use phoxal_cli_core::project::catalog::ArtifactKind;
use phoxal_cli_core::project::resolver::tool_participant_id;

pub fn run_check_with_context(
    resolved_platform_artifact_refs: &[PlatformArtifactRef],
    tool_participants: &[ToolParticipant],
    source_participants: &[SourceParticipant],
    context: CheckGraphContext<'_>,
    mut fetch: impl FnMut(&str) -> Result<RawParticipantReport>,
    mut fetch_tool: impl FnMut(&ToolParticipant) -> Result<RawParticipantReport>,
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
        let expected_artifact_id = if artifact.kind == ArtifactKind::Tool {
            tool_participant_id(&artifact.name)
        } else {
            &artifact.name
        };
        validate_artifact_identity(
            artifact.kind_label(),
            expected_artifact_id,
            artifact.kind.wire_kind(),
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

    for tool in tool_participants {
        let raw = fetch_tool(tool).with_context(|| {
            format!(
                "failed to obtain participant report for tool {} ({})",
                tool.name,
                tool.binary_path.display()
            )
        })?;
        let expected_id = tool_participant_id(&tool.name);
        validate_artifact_identity("tool", expected_id, "tool", &raw)?;
        let participant =
            graph_check::ParticipantApis::try_from(raw.clone()).with_context(|| {
                format!(
                    "failed to interpret participant report for tool {} ({})",
                    tool.name,
                    tool.binary_path.display()
                )
            })?;
        participants.push(participant);
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
        if participant.kind == SourceParticipantKind::ComponentDriver {
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
        } else if participant.kind == SourceParticipantKind::UserTool
            && let Some(problem) = crate::validation::validate_user_runtime_config(
                &participant.name,
                participant_apis.config_schema.as_ref(),
                context
                    .robot
                    .and_then(|robot| robot.tools.get(&participant.name))
                    .and_then(|tool| tool.config.as_ref()),
                "tools",
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
