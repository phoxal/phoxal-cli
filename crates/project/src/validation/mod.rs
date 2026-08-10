use crate::check as graph_check;
use phoxal_manifest::source::robot::v0::Manifest as RobotManifest;
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Arc;

use crate::RuntimeTarget;

use crate::Reporter;

pub use use_case::validate;

pub struct ValidateRequest {
    pub source: ValidationSource,
    pub offline: bool,
    pub reporter: Arc<dyn Reporter>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValidationSource {
    Project(RuntimeTarget),
    Archive(ArchiveValidation),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchiveValidation {
    pub archive: PathBuf,
    pub destination: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ValidationComponent {
    pub instance: String,
    pub source: String,
    pub has_driver: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ValidationReport {
    pub robot_path: PathBuf,
    pub robot: String,
    pub train: String,
    pub platform_services: Vec<String>,
    pub services: Vec<String>,
    pub components: Vec<ValidationComponent>,
}

/// The CLI's own participant-report shape: `artifact.id` IS self-reported -
/// a built binary's linker section carries the participant's own declared
/// `id`, kind, class, and config schema (see
/// `crate::check::participant_metadata`) - and is checked against
/// the expectations that selected the binary before any metadata fact is
/// trusted (`raw_participant_report_from_extracted_metadata`).
#[derive(Debug, Clone, PartialEq)]
pub struct RawParticipantReport {
    pub artifact: RawArtifact,
    pub config_schema: Option<Value>,
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
    pub robot: Option<&'a RobotManifest>,
}

impl CheckOutcome {
    #[must_use]
    pub fn is_ok(&self) -> bool {
        self.report.is_ok()
    }
}

mod participants;
mod use_case;

pub(crate) use participants::{
    PlatformArtifactRef, check_artifact_refs_from_resolved, component_driver_runtimes_by_ref,
    source_participants_from_resolved, source_participants_from_resolved_with_drivers,
};
mod graph;
pub use graph::run_check_with_context;
mod config;
pub(crate) use config::validate_user_service_config;
mod metadata;
pub(crate) use metadata::{
    extract_participant_report_from_staged_runtime, raw_participant_report_from_extracted_metadata,
};
mod build;
pub(crate) use build::{
    build_participant_report_from_binary, ensure_brain_declares_unit_config,
    validate_artifact_identity, validate_source_artifact_identity,
};
mod errors;
pub(crate) use errors::ensure_check_outcome_ok;

#[cfg(test)]
mod tests;
