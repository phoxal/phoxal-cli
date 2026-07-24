use clap::Args;
use phoxal::check as graph_check;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use phoxal::model::robot::RobotV0;
use phoxal_cli_core::check::participant_metadata;
use phoxal_cli_core::check::source::{
    SourceParticipant, ToolParticipant, UserServiceImageParticipant,
};

#[derive(Debug, Args)]
pub struct CheckCmd {
    #[arg(
        long,
        value_name = "TRIPLE",
        help = "Resolve official artifacts for this target instead of the host (e.g. aarch64, x86_64, or a full triple). Use it to validate a Linux robot from a non-Linux host."
    )]
    pub target: Option<String>,
    #[arg(long, help = "Promote coherence warnings to a failing check result.")]
    pub strict: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CheckOptions {
    pub suite_source: Option<String>,
    pub target: Option<String>,
    pub strict: bool,
}

/// The CLI's own participant-report shape: known artifact identity (never
/// self-reported anymore - a built binary's linker section carries only its
/// contracts, see [`participant_metadata`]) plus the extracted
/// contract list. No `bus_abi` (D1, X-tools slice: dissolved into the
/// version-qualified contract key, `phoxal::check::ParticipantApis` no
/// longer carries it either).
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct RawEmitApis {
    pub artifact: RawArtifact,
    #[serde(default = "default_participant_class")]
    pub participant_class: String,
    pub api_version: String,
    pub required_contracts: Vec<participant_metadata::ParticipantMetaContract>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_schema: Option<Value>,
}

fn default_participant_class() -> String {
    "checked".to_string()
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct RawArtifact {
    pub kind: String,
    pub id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckOutcome {
    pub missing_images: Vec<String>,
    pub report: graph_check::Report,
    pub checked_participants: Vec<graph_check::ParticipantApis>,
    pub contract_surfaces: Vec<graph_check::ParticipantContractSurface>,
}

#[derive(Debug, Clone, Copy)]
pub struct CheckGraphContext<'a> {
    pub robot: Option<&'a RobotV0>,
}

#[derive(Debug, Clone, Copy)]
pub struct CheckParticipants<'a> {
    pub platform_artifact_refs: &'a [PlatformArtifactRef],
    pub user_service_images: &'a [UserServiceImageParticipant],
    pub tool_participants: &'a [ToolParticipant],
    pub source_participants: &'a [SourceParticipant],
}

impl CheckOutcome {
    #[must_use]
    pub fn is_ok(&self) -> bool {
        self.missing_images.is_empty() && self.report.is_ok()
    }
}

mod coherence;
#[cfg(test)]
use coherence::{CoherenceDisposition, coherence_disposition};
pub use coherence::{CoherenceMismatchDiagnostic, RobotCoherenceDiagnostic};
pub(crate) use coherence::{
    CoherenceVerb, coherence_for_launch_plan, enforce_coherence, evaluate_robot_coherence,
    robot_contract_surfaces,
};
mod command;
pub use command::PlatformArtifactRef;
use command::run;
mod participants;
pub(crate) use participants::{
    check_artifact_refs_from_resolved, component_driver_runtimes_by_ref, ensure_suite_availability,
    source_participants_from_resolved, tool_participants_from_resolved,
};
mod graph;
pub use graph::{run_check, run_check_with_context, run_check_with_deployed_user_service_images};
mod config;
pub(crate) use config::{contract_surface, validate_user_service_config};
mod metadata;
pub(crate) use metadata::{
    extract_emit_apis_from_staged_runtime, extract_emit_apis_from_staged_tool,
    fetch_emit_apis_from_tool, raw_emit_apis_from_extracted_metadata, tool_env_override,
};
mod build;
#[cfg(test)]
pub(crate) use build::build_emit_apis_by_building;
pub(crate) use build::{
    build_emit_apis_from_source, build_emit_apis_from_source_for_check, validate_artifact_identity,
    validate_service_artifact_identity, validate_source_artifact_identity,
};
mod errors;
pub use errors::MissingImageError;
pub(crate) use errors::ensure_check_outcome_ok;

#[cfg(test)]
mod tests;
