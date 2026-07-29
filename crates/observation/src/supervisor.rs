use phoxal_cli_core::identity::ExecutionId;
use phoxal_cli_core::runtime::{ProjectLifecycle, SimulationSessionInfo, StartupStatus};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SupervisorObservation {
    pub supervisor_generation: u64,
    pub revision: u64,
    pub execution_id: ExecutionId,
    pub project: String,
    pub entry: String,
    pub framework_train: String,
    pub simulation: Option<SimulationSessionInfo>,
    pub lifecycle: ProjectLifecycle,
    pub router: String,
    pub plan_revision: u64,
    pub graph_generation: u64,
    pub startup: StartupStatus,
}
