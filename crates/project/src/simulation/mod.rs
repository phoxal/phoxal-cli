pub(crate) mod participants;
pub(crate) mod prepare;
pub(crate) mod resolve;
mod use_case;
pub(crate) mod webots;
pub(crate) mod world;

pub use use_case::{prepare_simulation, stage_webots};

use std::path::PathBuf;
use std::sync::Arc;

use crate::RuntimeTarget;
use phoxal_runtime_contract::identity::ExecutionId;

use crate::Reporter;

pub struct PrepareSimulationRequest {
    pub target: RuntimeTarget,
    pub world: String,
    /// Where the simulation release gets the `phoxald` that runs it.
    pub executor: crate::deployment::SharedExecutorSource,
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
pub struct PreparedSimulation {
    pub project_root: PathBuf,
    pub world_source: PathBuf,
    pub webots_executable: PathBuf,
}

pub struct StageWebotsRequest {
    pub staged_root: PathBuf,
    pub project_root: PathBuf,
    pub world_source: PathBuf,
    pub webots_executable: PathBuf,
    pub execution: ExecutionId,
    pub endpoint: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebotsLaunch {
    pub executable: PathBuf,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub world: PathBuf,
}

pub(crate) use participants::{
    ensure_exactly_one_simulator, official_simulator_participants, remap_simulator_participant_ids,
    sim_checked_participants,
};
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct SimulateOptions {
    pub(crate) world: String,
    pub(crate) offline: bool,
}

#[derive(Debug)]
pub(crate) struct ResolvedSimulation {
    pub(crate) project_root: std::path::PathBuf,
    pub(crate) world_path: std::path::PathBuf,
    pub(crate) resolved: crate::source::resolver::BundlePlan,
}
