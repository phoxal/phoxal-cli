//! Resident process and runtime-authority ownership for the Phoxal CLI.
//!
//! This crate owns project-operation locks, resident bootstrap and protocol
//! serving, child-process supervision, router readiness, exact participant
//! readiness, and systemd notification. It never depends on the disposable
//! client or terminal UI.

#![allow(clippy::module_name_repetitions)]

use std::time::{Duration, Instant};

const MAX_CAPTURED_LINE_BYTES: usize = phoxal_cli_core::session::MAX_ROUTED_LOG_TEXT_CHARS * 4;
const ROUTER_READY_TIMEOUT: Duration = Duration::from_secs(15);

/// A readiness/stage-wait budget for one supervised session.
///
/// This is explicit rather than a large duration sentinel: an interactive
/// operator may wait indefinitely, while a headless invocation needs a
/// deterministic deadline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitBudget {
    /// No deadline is applied.
    Unbounded,
    /// Fail after the supplied duration.
    Bounded(Duration),
}

impl Default for WaitBudget {
    /// Default to an already-elapsed bounded wait so a derived default can
    /// never silently turn a missing policy into an unbounded wait.
    fn default() -> Self {
        Self::Bounded(Duration::default())
    }
}

impl WaitBudget {
    /// Resolve this policy into the absolute deadline used by a wait loop.
    #[must_use]
    pub fn deadline_from(self, now: Instant) -> Option<Instant> {
        match self {
            Self::Unbounded => None,
            Self::Bounded(duration) => Some(now + duration),
        }
    }
}

pub mod lock;
pub mod resident;
pub mod router;
pub mod systemd;

mod process;
mod state;

pub use phoxal_cli_core::runtime::{ParticipantSpec, RestartPolicy};
pub use phoxal_cli_core::session::ProcessState;

pub use lock::{
    ProjectLock, ProjectLockIdentity, ProjectLockStatus, ProjectOperation, active_execution,
};
pub(crate) use process::child::ManagedChild;
pub use process::child::{materialize_plan_binaries, maybe_run_guardian};
pub use process::spec::{
    RequestedStop, SupervisorAction, SupervisorActionReceiver, SupervisorOptions,
};
pub use process::stages::{SupervisionStage, stages_for_run, stages_for_simulation};
pub use process::supervise::supervise_until_shutdown;
pub use router::{InfrastructureRouter, start_infrastructure_router};
pub use state::SupervisorState;
pub use state::readiness::{default_connect_endpoint, start_liveliness_observer};
