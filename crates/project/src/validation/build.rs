//! Build responsibilities for check.

use super::{RawParticipantReport, raw_participant_report_from_extracted_metadata};
use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use phoxal_cli_core::check as graph_check;
use phoxal_cli_core::check::participant_metadata;
use phoxal_cli_core::check::source::SourceParticipant;
use phoxal_cli_core::check::source::SourceParticipantKind;
use std::path::Path;

/// Read a source participant's report from the exact executable Cargo selected
/// for the preparation plan. Preparation builds a compatible group once, then
/// both graph checking and staging consume this path without asking Cargo again.
pub(crate) fn build_participant_report_from_binary(
    participant: &SourceParticipant,
    binary_path: &Path,
    reporter: &dyn crate::Reporter,
) -> Result<RawParticipantReport> {
    let meta =
        participant_metadata::extract_participant_metadata(binary_path).with_context(|| {
            format!(
                "failed to extract participant metadata for {} from {}",
                participant.name,
                binary_path.display()
            )
        })?;
    report_source_build_progress(
        Some(reporter),
        format!(
            "checked selected {} {} from {}",
            participant.kind_label(),
            participant.name,
            binary_path.display()
        ),
    );
    raw_participant_report_from_extracted_metadata(
        expected_kind_for_source_participant(participant.kind),
        &participant.expected_artifact_id,
        binary_path,
        meta,
    )
}

pub(crate) fn report_source_build_progress(ui: Option<&dyn crate::Reporter>, message: String) {
    if let Some(ui) = ui {
        ui.info(message);
    }
}

/// The expected `artifact.kind` label for a [`SourceParticipant`]'s kind.
pub(crate) fn expected_kind_for_source_participant(kind: SourceParticipantKind) -> &'static str {
    kind.shared_kind().label()
}

pub(crate) fn validate_source_artifact_identity(
    participant: &SourceParticipant,
    raw: &RawParticipantReport,
) -> Result<()> {
    validate_artifact_identity(
        participant.kind_label(),
        participant.expected_artifact_id.as_str(),
        expected_kind_for_source_participant(participant.kind),
        raw,
    )
}

pub(crate) fn validate_artifact_identity(
    label: &str,
    expected_id: &str,
    expected_kind: &str,
    raw: &RawParticipantReport,
) -> Result<()> {
    if raw.artifact.id != expected_id {
        bail!(
            "{label} participant report artifact.id '{}' does not match expected artifact id '{}'",
            raw.artifact.id,
            expected_id
        );
    }
    if raw.artifact.kind != expected_kind {
        bail!(
            "{label} participant report artifact.kind '{}' does not match the expected kind '{}'",
            raw.artifact.kind,
            expected_kind
        );
    }
    Ok(())
}

impl TryFrom<RawParticipantReport> for graph_check::ParticipantApis {
    type Error = anyhow::Error;

    fn try_from(raw: RawParticipantReport) -> Result<Self> {
        let artifact_id = raw.artifact.id;
        let participant_kind = graph_check::ParticipantKind::parse(&raw.artifact.kind);
        let participant_class = graph_check::ParticipantClass::parse(&raw.participant_class)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "participant report carries unknown class '{}'",
                    raw.participant_class
                )
            })?;
        Ok(Self {
            // Default the participant id to the artifact id; callers that launch
            // one artifact per instance (component drivers) override it with the
            // concrete instance id below.
            participant_id: artifact_id.clone(),
            artifact_id,
            participant_kind,
            participant_class,
            config_schema: raw.config_schema,
            scope: graph_check::ParticipantScope::Graph,
        })
    }
}
