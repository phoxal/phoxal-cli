//! Metadata responsibilities for check.

use super::{RawArtifact, RawParticipantReport, default_participant_class};
use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use phoxal_cli_core::check::participant_metadata;
use phoxal_cli_core::check::source::ToolParticipant;
use phoxal_cli_core::project::resolver::{ResolvedPlatformRuntime, official_binary_name};
use std::path::Path;
use std::path::PathBuf;

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

pub(crate) fn extract_participant_report_from_staged_tool(
    bin_dir: &Path,
    tool: &phoxal_cli_core::project::resolver::ResolvedTool,
) -> Result<RawParticipantReport> {
    let binary = bin_dir.join(&tool.binary_name);
    let meta = participant_metadata::extract_participant_metadata(&binary).with_context(|| {
        format!(
            "failed to extract participant metadata from {}",
            binary.display()
        )
    })?;
    raw_participant_report_from_extracted_metadata(
        "tool",
        phoxal_cli_core::project::resolver::tool_participant_id(&tool.name),
        &binary,
        meta,
    )
}

/// Every native tool (`tool-router`, `tool-joypad`) is privileged (host/root
/// access); every other kind is a checked participant. Neither the catalog
/// nor a binary's extracted metadata carries `participant_class` anymore, so
/// the kind -> class mapping (always fixed) is derived here instead of read
/// off either source.
pub(crate) fn default_participant_class_for_kind(artifact_kind: &str) -> String {
    if artifact_kind == "tool" {
        "privileged".to_string()
    } else {
        default_participant_class()
    }
}

/// Fetches a native tool binary's config-schema report by extracting its
/// compiled-in participant metadata section directly from the built artifact
/// file, never by executing it. The section carries the
/// participant's own declared `id` and its config schema, but not an
/// artifact `kind` label; the kind is supplied from what is already known
/// about `tool`, and the declared `id` is checked against `tool`'s own
/// expected identity before the schema is trusted (see
/// [`raw_participant_report_from_extracted_metadata`]).
pub(crate) fn fetch_participant_report_from_tool(
    tool: &ToolParticipant,
) -> Result<RawParticipantReport> {
    let meta = participant_metadata::extract_participant_metadata(&tool.binary_path).with_context(
        || {
            format!(
                "failed to extract participant metadata from {}",
                tool.binary_path.display()
            )
        },
    )?;
    raw_participant_report_from_extracted_metadata(
        "tool",
        phoxal_cli_core::project::resolver::tool_participant_id(&tool.name),
        &tool.binary_path,
        meta,
    )
}

/// Builds a [`RawParticipantReport`] from a binary's extracted [`ParticipantMeta`] plus
/// the artifact identity the caller already expects - the shared tail of
/// [`fetch_participant_report_from_tool`] and
/// [`build_participant_report_by_building`](super::build::build_participant_report_by_building).
///
/// The embedded metadata carries the participant's own declared `id` and its
/// config schema. That `id` is the participant's own truth about which artifact this
/// binary implements - the caller's `expected_artifact_id` is a claim made
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
    Ok(RawParticipantReport {
        artifact: RawArtifact {
            kind: artifact_kind.to_string(),
            id: meta.id,
        },
        participant_class: default_participant_class_for_kind(artifact_kind),
        config_schema: Some(meta.config_schema),
    })
}

pub(crate) fn tool_env_override(
    tool: &phoxal_cli_core::project::resolver::ResolvedTool,
) -> Option<PathBuf> {
    env_path_override("PHOXAL_ARTIFACT", &tool.name)
        .or_else(|| env_path_override("PHOXAL_TOOL", &tool.name))
        .or_else(|| {
            std::env::var_os("PHOXAL_ARTIFACT_DIR")
                .map(PathBuf::from)
                .map(|dir| dir.join(&tool.binary_name))
                .filter(|path| path.is_file())
        })
        .or_else(|| {
            std::env::var_os("PHOXAL_TOOL_DIR")
                .map(PathBuf::from)
                .and_then(|dir| {
                    [tool.name.as_str(), tool.binary_name.as_str()]
                        .into_iter()
                        .map(|name| dir.join(name))
                        .find(|path| path.is_file())
                })
        })
}

pub(crate) fn env_path_override(prefix: &str, id: &str) -> Option<PathBuf> {
    let key = format!("{prefix}_{}_PATH", env_key(id));
    std::env::var_os(key)
        .map(PathBuf::from)
        .filter(|path| path.is_file())
}

pub(crate) fn env_key(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(id: &str) -> participant_metadata::ParticipantMeta {
        participant_metadata::ParticipantMeta {
            id: id.to_string(),
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
