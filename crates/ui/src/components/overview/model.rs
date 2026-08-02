use std::collections::BTreeMap;
use std::sync::Arc;

use phoxal_cli_observation::{
    ConnectionObservation, Freshness, InputObservation, ProcessTable, SourceHealth,
    SupervisorObservation,
};

#[derive(Debug, Clone, Default, PartialEq)]
pub struct OverviewModel {
    pub connection: Option<ConnectionObservation>,
    pub supervisor: Option<Arc<SupervisorObservation>>,
    pub processes: Arc<ProcessTable>,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diagnostics_are_a_finite_presentation_window() {
        let mut model = OverviewModel::default();
        for index in 0..300 {
            model.push_diagnostic(index.to_string());
        }
        assert_eq!(model.diagnostics.len(), 256);
        assert_eq!(model.diagnostics.first().map(String::as_str), Some("44"));
        assert_eq!(model.diagnostics.last().map(String::as_str), Some("299"));
    }
}
