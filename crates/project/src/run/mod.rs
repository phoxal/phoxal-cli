pub(crate) mod participants;
pub(crate) mod prepare;
pub(crate) mod report;

pub use prepare::prepare_run;

use std::sync::Arc;

use crate::Reporter;
use crate::RuntimeTarget;

/// Prepare one execution's deployment release.
///
/// There is no driver or simulation option here. One bundle serves every mode:
/// which of its runtimes are actually started is the launcher's decision at
/// launch time, and nothing about it is written into the bundle.
pub struct PrepareRunRequest {
    pub target: RuntimeTarget,
    pub offline: bool,
    pub reporter: Arc<dyn Reporter>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedExecution {
    /// The verified deployment release to launch: the supervisor and the bundle
    /// it runs, which always come from the same release.
    pub release: crate::deployment::ReleaseLayout,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RunOptions {
    pub(crate) offline: bool,
}
