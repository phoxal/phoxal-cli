//! Metadata responsibilities for check.

use super::{RawArtifact, RawParticipantReport};
use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use phoxal_cli_core::check::participant_metadata;
use phoxal_cli_core::project::resolver::{ResolvedPlatformRuntime, official_binary_name};
use std::path::Path;

/// Extract one official runtime's participant report straight from its
/// already-materialized binary at `bin_dir/<canonical name>`. Materialization
/// (`cargo install`, or a source override build) is the caller's
/// responsibility - this never fetches or builds anything itself.
pub(crate) fn extract_participant_report_from_staged_runtime(
    bin_dir: &Path,
    runtime: &ResolvedPlatformRuntime,
) -> Result<RawParticipantReport> {
    let binary = bin_dir.join(official_binary_name(runtime.kind, &runtime.name));
    let meta = participant_metadata::extract_participant_metadata(&binary).with_context(|| {
        format!(
            "failed to extract participant metadata from {}",
            binary.display()
        )
    })?;
    raw_participant_report_from_extracted_metadata(
        runtime.kind.wire_kind(),
        &runtime.name,
        &binary,
        meta,
    )
}

/// Builds a [`RawParticipantReport`] from a binary's extracted [`ParticipantMeta`] plus
/// the artifact identity the caller already expects - the shared tail of staged
/// runtime and selected-source artifact inspection.
///
/// The embedded metadata carries the participant's declared identity, kind, and
/// config schema. The caller's expectations are claims made
/// from context (a resolved runtime name, a registry package, an
/// `expected_artifact_id` field) that could disagree with it, for instance if
/// two staged binaries were swapped on disk. This function is the one place
/// that reconciles the two: a mismatch fails here, naming both values and
/// `binary_path`, BEFORE the extracted config schema is ever used to validate
/// anything. On success the returned
/// [`RawArtifact::id`] is the binary's own declared `id`, not a copy of the
/// caller's expectation - by this point the two are known to agree.
///
/// [`ParticipantMeta`]: participant_metadata::ParticipantMeta
pub(crate) fn raw_participant_report_from_extracted_metadata(
    artifact_kind: &str,
    expected_artifact_id: &str,
    binary_path: &Path,
    meta: participant_metadata::ParticipantMeta,
) -> Result<RawParticipantReport> {
    if meta.id != expected_artifact_id {
        bail!(
            "{} at {} declares participant id '{}', but it was selected as the {artifact_kind} \
             artifact '{expected_artifact_id}'; the staged/built binary does not match the \
             identity that selected it",
            artifact_kind,
            binary_path.display(),
            meta.id,
        );
    }
    let kind = match meta.kind {
        phoxal_runtime_contract::ParticipantKind::Service => "service",
        phoxal_runtime_contract::ParticipantKind::Driver => "driver",
        phoxal_runtime_contract::ParticipantKind::Simulator => "simulator",
    };
    if kind != artifact_kind {
        bail!(
            "{} at {} declares participant kind '{}', but it was selected as a '{}' artifact; \
             the staged/built binary does not match the kind that selected it",
            meta.id,
            binary_path.display(),
            kind,
            artifact_kind,
        );
    }
    Ok(RawParticipantReport {
        artifact: RawArtifact {
            kind: kind.to_string(),
            id: meta.id,
        },
        config_schema: Some(meta.config_schema),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(id: &str) -> participant_metadata::ParticipantMeta {
        participant_metadata::ParticipantMeta {
            schema: phoxal_runtime_contract::PARTICIPANT_METADATA_SCHEMA.to_string(),
            id: id.to_string(),
            kind: phoxal_runtime_contract::ParticipantKind::Service,
            config_schema: serde_json::json!({"type": "object", "properties": {"speed": {"type": "integer"}}}),
        }
    }

    /// The binary's own declared `id` must be compared against the identity
    /// that selected it, not discarded in favor of trusting the caller's
    /// expectation unconditionally.
    #[test]
    fn matching_id_passes_and_carries_the_binarys_declared_id_and_schema() -> Result<()> {
        let raw = raw_participant_report_from_extracted_metadata(
            "service",
            "drive",
            Path::new("bin/phoxal-service-drive"),
            meta("drive"),
        )?;
        assert_eq!(raw.artifact.id, "drive");
        assert_eq!(raw.artifact.kind, "service");
        assert_eq!(
            raw.config_schema,
            Some(
                serde_json::json!({"type": "object", "properties": {"speed": {"type": "integer"}}})
            )
        );
        Ok(())
    }

    #[test]
    fn mismatched_kind_fails_before_metadata_enters_the_graph() {
        let error = raw_participant_report_from_extracted_metadata(
            "driver",
            "drive",
            Path::new("bin/phoxal-component-drive"),
            meta("drive"),
        )
        .expect_err("a binary declaring the wrong kind must be rejected")
        .to_string();
        assert!(error.contains("service"), "{error}");
        assert!(error.contains("driver"), "{error}");
    }

    #[test]
    fn mismatched_id_fails_naming_both_values_and_the_binary_path() {
        let error = raw_participant_report_from_extracted_metadata(
            "service",
            "drive",
            Path::new("bin/phoxal-service-drive"),
            meta("mission"),
        )
        .expect_err("a binary declaring the wrong id must be rejected");
        let message = error.to_string();
        assert!(message.contains("mission"), "{message}");
        assert!(message.contains("drive"), "{message}");
        assert!(message.contains("bin/phoxal-service-drive"), "{message}");
    }

    /// A binary with no metadata section at all must be a hard error, not a
    /// synthesized identity that then trivially "matches" nothing - covered at
    /// the extraction layer itself:
    /// `phoxal_cli_core::check::participant_metadata::tests::foreign_object_without_section_is_a_clear_error`.
    /// Here we cover the same shape one layer up: `extract_participant_metadata`
    /// on a file that is not a recognized object file at all fails before any
    /// identity comparison is attempted.
    #[test]
    fn missing_metadata_extraction_fails_before_any_identity_comparison() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let not_an_object_file = dir.path().join("not-a-binary");
        std::fs::write(&not_an_object_file, b"not an object file")?;
        let error = participant_metadata::extract_participant_metadata(&not_an_object_file)
            .expect_err("a non-object file must fail extraction");
        assert!(
            error.to_string().contains("not a recognized object file"),
            "{error}"
        );
        Ok(())
    }
}
