//! Metadata responsibilities for check.

use super::{RawArtifact, RawEmitApis, default_participant_class};
use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use phoxal_cli_core::check::participant_metadata;
use phoxal_cli_core::check::source::ToolParticipant;
use phoxal_cli_core::project::resolver::ResolvedPlatformRuntime;
use std::path::PathBuf;

pub(crate) fn extract_emit_apis_from_staged_runtime(
    runtime: &ResolvedPlatformRuntime,
) -> Result<RawEmitApis> {
    #[cfg(test)]
    if runtime
        .url
        .as_deref()
        .is_some_and(|url| url.starts_with("https://example.invalid/"))
    {
        return Ok(raw_emit_apis_from_extracted_metadata(
            runtime.kind.emit_apis_kind(),
            &runtime.name,
            participant_metadata::ParticipantMeta {
                id: "fixture".to_string(),
                config_schema: serde_json::json!({ "type": "null" }),
            },
        ));
    }
    let binary = crate::native_artifacts::stage_runtime(
        None,
        runtime,
        phoxal_cli_core::artifacts::ProvisioningMode::MissingOnly,
    )?
    .ok_or_else(|| anyhow!("{} has no staged binary", runtime.package))?;
    let meta = participant_metadata::extract_participant_metadata(&binary)
        .with_context(|| format!("failed to extract API metadata from {}", binary.display()))?;
    Ok(raw_emit_apis_from_extracted_metadata(
        runtime.kind.emit_apis_kind(),
        &runtime.name,
        meta,
    ))
}

pub(crate) fn extract_emit_apis_from_staged_tool(
    tool: &phoxal_cli_core::project::resolver::ResolvedTool,
) -> Result<RawEmitApis> {
    #[cfg(test)]
    if !tool.published
        || tool
            .url
            .as_deref()
            .is_some_and(|url| url.starts_with("https://example.invalid/"))
    {
        return Ok(raw_emit_apis_from_extracted_metadata(
            "tool",
            phoxal_cli_core::project::resolver::tool_emit_apis_id(&tool.name),
            participant_metadata::ParticipantMeta {
                id: "fixture".to_string(),
                config_schema: serde_json::json!({ "type": "null" }),
            },
        ));
    }
    let binary = crate::native_artifacts::stage_tool(
        None,
        tool,
        phoxal_cli_core::artifacts::ProvisioningMode::MissingOnly,
    )?
    .ok_or_else(|| anyhow!("{} has no staged binary", tool.package))?;
    let meta = participant_metadata::extract_participant_metadata(&binary)
        .with_context(|| format!("failed to extract API metadata from {}", binary.display()))?;
    Ok(raw_emit_apis_from_extracted_metadata(
        "tool",
        phoxal_cli_core::project::resolver::tool_emit_apis_id(&tool.name),
        meta,
    ))
}

/// Every native tool (`tool-router`, `tool-joypad`) is privileged (host/root
/// access); every other kind is a checked participant. Neither the suite
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
/// compiled-in `#[derive(phoxal::Api)]` metadata section directly from the
/// built artifact file - never by executing it (the `emit-apis` runtime
/// subcommand this used to run is gone). A binary's own linker section
/// carries no artifact identity (`kind`/`id`), so the identity is supplied
/// from what is already known about `tool`; the config schema comes from the
/// section.
pub(crate) fn fetch_emit_apis_from_tool(tool: &ToolParticipant) -> Result<RawEmitApis> {
    let meta = participant_metadata::extract_participant_metadata(&tool.binary_path).with_context(
        || {
            format!(
                "failed to extract API metadata from {}",
                tool.binary_path.display()
            )
        },
    )?;
    Ok(raw_emit_apis_from_extracted_metadata(
        "tool",
        phoxal_cli_core::project::resolver::tool_emit_apis_id(&tool.name),
        meta,
    ))
}

/// Builds a [`RawEmitApis`] from a binary's extracted [`ParticipantMeta`] plus
/// the artifact identity the caller already knows - the shared tail of
/// [`fetch_emit_apis_from_tool`] and [`build_emit_apis_by_building`]. The
/// embedded metadata carries no contract inventory anymore (organization#957:
/// there is no API-coherence pass left to feed one); what it does carry is the
/// participant's own declared `id` and its config schema.
///
/// [`ParticipantMeta`]: participant_metadata::ParticipantMeta
pub(crate) fn raw_emit_apis_from_extracted_metadata(
    artifact_kind: &str,
    artifact_id: &str,
    meta: participant_metadata::ParticipantMeta,
) -> RawEmitApis {
    RawEmitApis {
        artifact: RawArtifact {
            kind: artifact_kind.to_string(),
            id: artifact_id.to_string(),
        },
        participant_class: default_participant_class_for_kind(artifact_kind),
        config_schema: Some(meta.config_schema),
    }
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
