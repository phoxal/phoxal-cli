//! Webots simulation staging.
//!
//! Simulation is a *launch* decision, never a bundle fact. The bundle a
//! simulation runs is byte-identical to the one `phoxal run` and `phoxal build`
//! produce: one manifest, every driver block intact, every binary staged. What
//! this module adds is the world Webots opens and the controller that drives
//! it - a host tool on its own release train, materialized into the CLI's cache
//! and copied into the disposable Webots project, never into the bundle.

pub(crate) mod controller_tool;
pub(crate) mod prepare;
mod use_case;
pub(crate) mod webots;
pub(crate) mod world;

pub use use_case::{prepare_simulation, stage_webots};

use std::path::PathBuf;
use std::sync::Arc;

use crate::Reporter;
use crate::RuntimeTarget;

pub struct PrepareSimulationRequest {
    pub target: RuntimeTarget,
    pub world: String,
    pub offline: bool,
    pub webots: WebotsHost,
    pub reporter: Arc<dyn Reporter>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebotsHost {
    pub executable: PathBuf,
    pub home: PathBuf,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PreparedSimulation {
    pub project_root: PathBuf,
    pub world_source: PathBuf,
    pub webots_executable: PathBuf,
    /// The materialized Webots controller this session will stage into its
    /// disposable Webots project. It is a host tool, so it is resolved once
    /// during preparation and never enters the bundle.
    pub controller: PathBuf,
}

/// Everything Webots staging needs. There is no execution id and no participant
/// id: the controller learns the execution from the router it dials, and it
/// declares one liveliness token per component instance it simulates rather
/// than being launched as a named participant. That is also why the world can
/// be staged before the supervisor exists.
pub struct StageWebotsRequest {
    pub bundle_root: PathBuf,
    pub project_root: PathBuf,
    pub world_source: PathBuf,
    pub webots_executable: PathBuf,
    pub controller: PathBuf,
    pub endpoint: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebotsLaunch {
    pub executable: PathBuf,
    pub args: Vec<String>,
    pub world: PathBuf,
}
