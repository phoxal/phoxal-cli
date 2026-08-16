//! Build responsibilities for check.

use super::{RawParticipantReport, raw_participant_report_from_extracted_metadata};
use crate::check as graph_check;
use crate::check::participant_metadata;
use crate::check::source::SourceParticipant;
use crate::check::source::SourceParticipantKind;
use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use phoxal_runtime_contract::version::FrameworkVersion;
use std::path::Path;

/// Read a source participant's report from the exact executable Cargo selected
/// for the preparation plan. Preparation builds a compatible group once, then
/// both graph checking and staging consume this path without asking Cargo again.
///
/// `project_framework` is the framework the project selected in its lockfile,
/// which is what the built binary's own train has to agree with.
pub(crate) fn build_participant_report_from_binary(
    participant: &SourceParticipant,
    binary_path: &Path,
    project_framework: FrameworkVersion,
    reporter: &dyn crate::Reporter,
) -> Result<RawParticipantReport> {
    let meta = participant_metadata::extract_participant_metadata_for_project(
        binary_path,
        project_framework,
    )
    .with_context(|| {
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
    match kind.shared_kind() {
        phoxal_runtime_contract::metadata::ParticipantKind::Brain => "brain",
        phoxal_runtime_contract::metadata::ParticipantKind::Service => "service",
        phoxal_runtime_contract::metadata::ParticipantKind::Driver => "driver",
    }
}

/// The root brain's `Config` is fixed to `()` by `#[phoxal::brain]`, so its
/// embedded schema is exactly the unit schema.
///
/// This is the check-engine half of that gate. Runtime-bundle verification
/// enforces the identical rule over a staged `bin/brain`, which covers
/// `run`/`start`/`build` and every extracted bundle; `phoxal validate` and the
/// Webots simulation path never open a layout, so without this check a root
/// binary hand-embedding a real config surface would pass validation and reach
/// the supervisor. A binary claiming any config at all is not a brain, whatever
/// its id and kind declare.
pub(crate) fn ensure_brain_declares_unit_config(
    participant: &SourceParticipant,
    raw: &RawParticipantReport,
) -> Result<()> {
    let unit = serde_json::json!({"type": "null"});
    let declared = raw.config_schema.as_ref();
    if declared == Some(&unit) {
        return Ok(());
    }
    bail!(
        "root brain {} at {} declares config schema {}, but the brain takes no config at all and \
         must declare {{\"type\":\"null\"}}; `#[phoxal::brain]` fixes `Config = ()`",
        participant.name,
        participant.crate_dir.display(),
        declared.map_or_else(|| "none".to_string(), ToString::to_string),
    )
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
        Ok(Self {
            // Default the participant id to the artifact id; callers that launch
            // one artifact per instance (component drivers) override it with the
            // concrete instance id below.
            participant_id: artifact_id.clone(),
            artifact_id,
            participant_kind,
            config_schema: raw.config_schema,
            scope: graph_check::ParticipantScope::Graph,
        })
    }
}

#[cfg(test)]
mod tests {
    use crate::check::source::SourceParticipant;
    use crate::validation::{
        CheckGraphContext, RawArtifact, RawParticipantReport, run_check_with_context,
    };
    use anyhow::{Result, bail};
    use std::path::PathBuf;

    /// A brain report carrying `schema` as its embedded config schema, so the
    /// check engine can be driven against a root binary that is not a valid brain.
    fn raw_brain(schema: Option<serde_json::Value>) -> RawParticipantReport {
        RawParticipantReport {
            artifact: RawArtifact {
                kind: "brain".to_string(),
                id: "brain".to_string(),
            },
            config_schema: schema,
        }
    }

    fn fixture_brain_source() -> SourceParticipant {
        SourceParticipant::brain(PathBuf::from("/tmp/robot"), "testbot-robot")
    }

    /// The check-engine half of the unit-config gate.
    ///
    /// `phoxal validate` and the Webots simulation path never open a staged
    /// layout, so runtime-bundle verification's identical rule cannot protect
    /// them. A root binary whose id and kind are a perfectly good `brain` but
    /// which embeds a real config surface must still fail here, before any
    /// supervisor startup, and the ordinary unit schema must still pass.
    #[test]
    fn a_brain_declaring_a_config_surface_fails_the_check_engine() -> Result<()> {
        let sources = vec![fixture_brain_source()];
        let check = |schema: Option<serde_json::Value>| {
            run_check_with_context(
                &[],
                &sources,
                CheckGraphContext { robot: None },
                |_| bail!("no platform artifact reports are fetched here"),
                |_| Ok(raw_brain(schema.clone())),
            )
        };

        let error = check(Some(serde_json::json!({
            "type": "object",
            "properties": {"speed": {"type": "integer"}},
        })))
        .expect_err("a brain embedding a real config surface must be rejected")
        .to_string();
        assert!(error.contains("brain"), "{error}");
        assert!(error.contains("takes no config at all"), "{error}");

        // A missing schema is equally not the unit schema.
        let error = check(None)
            .expect_err("a brain with no embedded config schema must be rejected")
            .to_string();
        assert!(error.contains("takes no config at all"), "{error}");

        // The real `#[phoxal::brain]` record passes and enters the graph.
        let outcome = check(Some(serde_json::json!({"type": "null"})))?;
        assert!(outcome.is_ok(), "{outcome:?}");
        assert_eq!(
            outcome
                .checked_participants
                .iter()
                .map(|participant| participant.participant_id.as_str())
                .collect::<Vec<_>>(),
            ["brain"]
        );
        Ok(())
    }
}
