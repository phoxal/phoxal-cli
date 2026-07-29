use phoxal_cli_core::session::{RobotScope, RuntimePerformanceSample};

use crate::{ObservationQuery, ObservationWindow, WindowDirection};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeQuery {
    pub participant: Option<String>,
    pub direction: WindowDirection,
    pub limit: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RuntimeRow {
    pub scope: RobotScope,
    pub sample: RuntimePerformanceSample,
    pub capacity_evictions: u64,
}

pub type RuntimeRead = ObservationQuery<RuntimeQuery>;
pub type RuntimeWindow = ObservationWindow<RuntimeRow>;
