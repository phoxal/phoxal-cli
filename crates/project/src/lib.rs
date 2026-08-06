//! Project loading, validation, build, materialization, and staging ownership.
//!
//! This crate will turn authored project inputs into validated runtime plans and
//! resolve logical roots into shared runtime targets. It must not parse CLI
//! arguments, render terminal output, or own resident/client lifecycle.

#![allow(clippy::module_name_repetitions)]

mod build;
mod bundle;
pub mod host;
mod load;
mod paths;
mod progress;
pub mod registry;
mod registry_package;
mod resolve;
mod run;
mod simulation;
mod stage;
mod target;
mod validation;

pub use build::container::ContainerEngine;
pub use paths::runtime::{
    ACTIVE_RUNTIME_ROOT, INSTALL_ROOT, INSTALLED_BINARY_ROOT, INSTALLED_CLIENT_BINARY,
    INSTALLED_DAEMON_BINARY, INSTALLED_STATE_ROOT, INSTALLED_VOLATILE_ROOT, RELEASES_ROOT,
    RuntimePaths, SYSTEMD_ACTIVE_ROOT, SYSTEMD_UNIT, SYSTEMD_UNIT_PATH, SYSTEMD_UNIT_ROOT,
};
pub use progress::{PhaseId, PhaseOutcome, PreparationEvent, Reporter, SilentReporter};

use phoxal_cli_core::project::launch_plan::{LaunchPlan, RunIdentity};
use phoxal_cli_core::runtime::{
    ParticipantKind, ParticipantState, ProcessKey, RobotKey, StartupRequirement,
};
use phoxal_cli_core::runtime::{ParticipantSpec, RuntimeTarget};
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DriverMode {
    On,
    Off,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriverRequest {
    pub mode: DriverMode,
    pub subset: Vec<String>,
}

pub struct PrepareRunRequest {
    pub target: RuntimeTarget,
    pub run: RunIdentity,
    pub drivers: DriverRequest,
    pub offline: bool,
    pub reporter: Arc<dyn Reporter>,
}

pub struct PrepareSimulationRequest {
    pub target: RuntimeTarget,
    pub run: RunIdentity,
    pub world: String,
    pub offline: bool,
    pub webots: WebotsHost,
    pub reporter: Arc<dyn Reporter>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebotsHost {
    pub executable: PathBuf,
    pub home: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PreparedParticipant {
    pub key: ProcessKey,
    pub id: String,
    pub kind: ParticipantKind,
    pub robot: Option<RobotKey>,
    pub local: bool,
    pub startup_requirement: StartupRequirement,
    pub initial_state: ParticipantState,
    pub note: Option<String>,
    pub launch: Option<ParticipantSpec>,
}

/// The embedded router's inputs. There is no binary: the router runs inside the
/// supervisor process (organization#978), so only the authored Zenoh config and
/// the endpoint participants dial survive staging.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedRouter {
    pub config: Option<PathBuf>,
    pub endpoint: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PreparedSimulation {
    pub world: PathBuf,
    pub stage_root: PathBuf,
    pub stop_first: ProcessKey,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PreparedExecution {
    pub target: RuntimeTarget,
    pub project_root: PathBuf,
    pub staged_root: PathBuf,
    pub train: String,
    pub plan: LaunchPlan,
    pub participants: Vec<PreparedParticipant>,
    pub router: PreparedRouter,
    pub simulation: Option<PreparedSimulation>,
}

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BuildBackend {
    Local {
        target: Option<String>,
    },
    Container {
        target: Option<String>,
        engine: ContainerEngine,
        image: Option<String>,
    },
    Ssh {
        host: String,
        target: Option<String>,
    },
}

pub struct BuildBundleRequest {
    pub target: RuntimeTarget,
    pub backend: BuildBackend,
    pub output: Option<PathBuf>,
    pub publish: bool,
    pub offline: bool,
    pub reporter: Arc<dyn Reporter>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuiltBundle {
    pub archive: PathBuf,
    pub sha256: String,
    pub staged_root: Option<PathBuf>,
}

pub fn resolve_target(
    explicit: Option<&std::path::Path>,
    fallback: &std::path::Path,
) -> anyhow::Result<RuntimeTarget> {
    target::resolve(explicit, fallback)
}

pub async fn prepare_run(request: PrepareRunRequest) -> anyhow::Result<PreparedExecution> {
    tokio::task::spawn_blocking(move || run::prepare::prepare_run(request)).await?
}

pub async fn build_bundle(request: BuildBundleRequest) -> anyhow::Result<BuiltBundle> {
    tokio::task::spawn_blocking(move || build::build_bundle(request)).await?
}

pub async fn validate(request: ValidateRequest) -> anyhow::Result<ValidationReport> {
    tokio::task::spawn_blocking(move || validation::validate(request)).await?
}

pub async fn prepare_simulation(
    request: PrepareSimulationRequest,
) -> anyhow::Result<PreparedExecution> {
    tokio::task::spawn_blocking(move || simulation::prepare_simulation(request)).await?
}
