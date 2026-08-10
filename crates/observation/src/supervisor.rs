//! The execution-level observation, projected from one supervisor snapshot.

use phoxal_api::supervisor::snapshot::{DaemonFailure, Lifecycle, StartupStep};
use phoxal_runtime_contract::clock::Clock;
use phoxal_runtime_contract::identity::{ExecutionId, RobotId};

/// What an attached client knows about the execution as a whole.
///
/// Everything here comes from the authoritative snapshot except `project`,
/// which is the client's own local knowledge of where the bundle it launched
/// lives - the daemon has no opinion about the operator's directory layout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SupervisorObservation {
    /// Monotonic within one execution. A client keeps the highest it has seen.
    pub revision: u64,
    pub execution: ExecutionId,
    pub robot: RobotId,
    pub clock: Clock,
    /// Where this client believes the execution's bundle lives, for display.
    pub project: String,
    pub lifecycle: Lifecycle,
    pub startup: Vec<StartupStep>,
    /// Why `lifecycle` reached `Failed`, as a typed reason plus its evidence;
    /// `None` for every other lifecycle.
    pub failure: Option<DaemonFailure>,
}
