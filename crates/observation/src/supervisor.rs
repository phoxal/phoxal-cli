//! The execution-level observation, projected from one supervisor snapshot.

use phoxal_client::supervisor::execution::{Lifecycle, StartupStep};
use phoxal_runtime_contract::identity::{ExecutionId, RobotId};

/// What an attached client knows about the execution as a whole.
///
/// Everything here comes from the authoritative snapshot except `project`,
/// which is the client's own local knowledge of where the bundle it launched
/// lives - the supervisor has no opinion about the operator's directory layout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SupervisorObservation {
    /// Monotonic within one execution. A client keeps the highest it has seen.
    pub revision: u64,
    pub execution: ExecutionId,
    pub robot: RobotId,
    /// Where this client believes the execution's bundle lives, for display.
    pub project: String,
    /// Derived entirely from presence: `Starting` until every expected runtime
    /// has been seen, `Ready` while they all are, `Degraded` once one is not.
    /// There is no failed or stopped value - the supervisor's own death is
    /// observed as its identity token disappearing.
    pub lifecycle: Lifecycle,
    pub startup: Vec<StartupStep>,
}
