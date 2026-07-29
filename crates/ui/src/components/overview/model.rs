use std::collections::BTreeMap;
use std::sync::Arc;

use phoxal_cli_observation::{
    DeviceObservation, Freshness, InputObservation, ProcessTable, SourceHealth,
    SupervisorObservation,
};

#[derive(Debug, Clone, Default, PartialEq)]
pub struct OverviewModel {
    pub supervisor: Option<Arc<SupervisorObservation>>,
    pub processes: Arc<ProcessTable>,
    pub devices: Option<Arc<DeviceObservation>>,
    pub input: Option<Arc<InputObservation>>,
    pub source_health: Option<Arc<SourceHealth>>,
    pub freshness: BTreeMap<String, Freshness>,
    pub diagnostics: Vec<String>,
}

impl OverviewModel {
    pub fn push_diagnostic(&mut self, message: String) {
        const LIMIT: usize = 256;
        if self.diagnostics.len() == LIMIT {
            self.diagnostics.remove(0);
        }
        self.diagnostics.push(message);
    }
}
