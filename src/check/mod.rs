use phoxal::check as graph_check;
use phoxal::model::robot::RobotV0;
use serde_json::Value;

/// The CLI's own participant-report shape: `artifact.id` IS self-reported -
/// a built binary's linker section carries the participant's own declared
/// `id` alongside its config schema (see
/// `phoxal_cli_core::check::participant_metadata`) - and is checked against
/// the identity that selected the binary before its schema is trusted
/// (`raw_participant_report_from_extracted_metadata`). `artifact.kind` is still
/// supplied by the caller: the section carries no kind label. No `bus_abi`
/// (D1, X-tools slice: dissolved into the version-qualified contract key,
/// `phoxal::check::ParticipantApis` no longer carries it either). No contract
/// inventory (organization#957): there is no API-coherence pass left to
/// feed.
#[derive(Debug, Clone, PartialEq)]
pub struct RawParticipantReport {
    pub artifact: RawArtifact,
    pub participant_class: String,
    pub config_schema: Option<Value>,
}

pub(crate) fn default_participant_class() -> String {
    "checked".to_string()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawArtifact {
    pub kind: String,
    pub id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckOutcome {
    pub report: graph_check::Report,
    pub checked_participants: Vec<graph_check::ParticipantApis>,
}

#[derive(Debug, Clone, Copy)]
pub struct CheckGraphContext<'a> {
    pub robot: Option<&'a RobotV0>,
}

impl CheckOutcome {
    #[must_use]
    pub fn is_ok(&self) -> bool {
        self.report.is_ok()
    }
}

mod participants;
pub(crate) use participants::{
    PlatformArtifactRef, check_artifact_refs_from_resolved, component_driver_runtimes_by_ref,
    source_participants_from_resolved, tool_participants_from_resolved,
};
mod graph;
pub use graph::run_check_with_context;
mod config;
pub(crate) use config::{validate_user_runtime_config, validate_user_service_config};
mod metadata;
pub(crate) use metadata::{
    extract_participant_report_from_staged_runtime, extract_participant_report_from_staged_tool,
    fetch_participant_report_from_tool, raw_participant_report_from_extracted_metadata,
    tool_env_override,
};
mod build;
pub(crate) use build::{
    build_participant_report_by_building, build_participant_report_from_source,
    build_participant_report_from_source_with_diagnostics, validate_artifact_identity,
    validate_source_artifact_identity,
};
mod errors;
pub(crate) use errors::ensure_check_outcome_ok;

#[cfg(test)]
mod tests;
