pub(crate) mod participants;
pub(crate) mod prepare;
pub(crate) mod report;

pub use prepare::prepare_run;
pub(crate) use report::DriverPolicy;

use std::sync::Arc;

use crate::RuntimeTarget;

use crate::Reporter;

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
    pub drivers: DriverRequest,
    pub offline: bool,
    pub reporter: Arc<dyn Reporter>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PreparedExecution {
    /// The verified deployment release to launch: the supervisor and the bundle
    /// it runs, which always come from the same release.
    pub release: crate::deployment::ReleaseLayout,
    pub simulation: Option<super::simulation::PreparedSimulation>,
}

pub(crate) type DriversMode = DriverMode;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RunOptions {
    pub(crate) drivers: DriversMode,
    pub(crate) drivers_subset: Vec<String>,
    pub(crate) offline: bool,
}
