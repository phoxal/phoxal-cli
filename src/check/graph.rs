//! Graph responsibilities for check.

use super::{
    CheckGraphContext, CheckOutcome, MissingImageError, PlatformArtifactRef, RawEmitApis,
    contract_surface, validate_artifact_identity, validate_source_artifact_identity,
    validate_user_service_config,
};
use anyhow::Context;
use anyhow::Result;
use phoxal::check as graph_check;
use phoxal_cli_core::check::source::SourceParticipant;
use phoxal_cli_core::check::source::SourceParticipantKind;
use phoxal_cli_core::check::source::ToolParticipant;
use phoxal_cli_core::project::resolver::tool_emit_apis_id;
use phoxal_cli_core::project::suite::ArtifactKind;

pub fn run_check(
    resolved_platform_image_refs: &[(String, String)],
    tool_participants: &[ToolParticipant],
    source_participants: &[SourceParticipant],
    fetch: impl FnMut(&str) -> Result<RawEmitApis>,
    fetch_tool: impl FnMut(&ToolParticipant) -> Result<RawEmitApis>,
    build: impl FnMut(&SourceParticipant) -> Result<RawEmitApis>,
) -> Result<CheckOutcome> {
    let platform_artifact_refs = service_platform_artifact_refs(resolved_platform_image_refs);
    run_check_with_context(
        &platform_artifact_refs,
        tool_participants,
        source_participants,
        CheckGraphContext { robot: None },
        fetch,
        fetch_tool,
        build,
    )
}

pub(super) fn service_platform_artifact_refs(
    resolved_platform_image_refs: &[(String, String)],
) -> Vec<PlatformArtifactRef> {
    resolved_platform_image_refs
        .iter()
        .map(|(name, artifact_ref)| PlatformArtifactRef {
            name: name.clone(),
            kind: ArtifactKind::Service,
            artifact_ref: artifact_ref.clone(),
            instances: Vec::new(),
        })
        .collect()
}

pub fn run_check_with_context(
    resolved_platform_artifact_refs: &[PlatformArtifactRef],
    tool_participants: &[ToolParticipant],
    source_participants: &[SourceParticipant],
    context: CheckGraphContext<'_>,
    mut fetch: impl FnMut(&str) -> Result<RawEmitApis>,
    mut fetch_tool: impl FnMut(&ToolParticipant) -> Result<RawEmitApis>,
    mut build: impl FnMut(&SourceParticipant) -> Result<RawEmitApis>,
) -> Result<CheckOutcome> {
    let mut missing_images = Vec::new();
    let mut participants = Vec::new();
    let mut contract_surfaces = Vec::new();
    let mut config_problems = Vec::new();

    for artifact in resolved_platform_artifact_refs {
        let image_ref = &artifact.artifact_ref;
        let raw = match fetch(image_ref) {
            Ok(raw) => raw,
            Err(error) if error.downcast_ref::<MissingImageError>().is_some() => {
                missing_images.push(image_ref.clone());
                continue;
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "failed to obtain emit-apis for {} {} ({image_ref})",
                        artifact.kind_label(),
                        artifact.name
                    )
                });
            }
        };
        let expected_artifact_id = if artifact.kind == ArtifactKind::Tool {
            tool_emit_apis_id(&artifact.name)
        } else {
            &artifact.name
        };
        validate_artifact_identity(
            artifact.kind_label(),
            expected_artifact_id,
            artifact.kind.emit_apis_kind(),
            &raw,
        )?;
        let participant =
            graph_check::ParticipantApis::try_from(raw.clone()).with_context(|| {
                format!(
                    "failed to interpret emit-apis for {} {} ({image_ref})",
                    artifact.kind_label(),
                    artifact.name
                )
            })?;
        if artifact.instances.is_empty() {
            contract_surfaces.push(contract_surface(&raw, artifact.name.clone()));
            participants.push(participant);
        } else {
            // A suite component driver is fetched once but launched once
            // per instance that declares it - key each instance's graph
            // membership by its own id, exactly like a workspace-built
            // driver source participant does (see
            // `SourceParticipantKind::ComponentDriver` below).
            for instance in &artifact.instances {
                let mut instance_participant = participant.clone();
                instance_participant.participant_id = instance.clone();
                instance_participant.scope =
                    graph_check::ParticipantScope::ComponentInstance(instance.clone());
                contract_surfaces.push(contract_surface(&raw, instance.clone()));
                participants.push(instance_participant);
            }
        }
    }

    for tool in tool_participants {
        let raw = fetch_tool(tool).with_context(|| {
            format!(
                "failed to obtain emit-apis for tool {} ({})",
                tool.name,
                tool.binary_path.display()
            )
        })?;
        let expected_id = tool_emit_apis_id(&tool.name);
        validate_artifact_identity("tool", expected_id, "tool", &raw)?;
        let participant =
            graph_check::ParticipantApis::try_from(raw.clone()).with_context(|| {
                format!(
                    "failed to interpret emit-apis for tool {} ({})",
                    tool.name,
                    tool.binary_path.display()
                )
            })?;
        contract_surfaces.push(contract_surface(&raw, tool.name.clone()));
        participants.push(participant);
    }

    for participant in source_participants {
        let raw = build(participant).with_context(|| {
            format!(
                "failed to obtain emit-apis for {} {} ({})",
                participant.kind_label(),
                participant.name,
                participant.crate_dir.display()
            )
        })?;
        validate_source_artifact_identity(participant, &raw)?;
        let mut participant_apis = graph_check::ParticipantApis::try_from(raw.clone())
            .with_context(|| {
                format!(
                    "failed to interpret emit-apis for {} {} ({})",
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
            && let Some(problem) = crate::check::validate_user_runtime_config(
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
        let surface_participant_id = if participant.kind == SourceParticipantKind::Tool {
            participant.name.clone()
        } else {
            participant_apis.participant_id.clone()
        };
        contract_surfaces.push(contract_surface(&raw, surface_participant_id));
        participants.push(participant_apis);
    }

    let mut report = graph_check::check_graph(&participants);
    report.problems.extend(config_problems);
    Ok(CheckOutcome {
        missing_images,
        report,
        checked_participants: participants,
        contract_surfaces,
    })
}
