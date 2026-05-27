use std::path::{Path, PathBuf};

use anyhow::Result;
use phoxal_utils_scenario_spec::{ScenarioOutcome, ScenarioReport};

#[derive(Debug, Clone, Eq, PartialEq)]
pub(crate) struct PhaseReport {
    pub(crate) selector: String,
    pub(crate) report: ScenarioReport,
}

impl PhaseReport {
    pub(crate) fn has_failures(&self) -> bool {
        self.report.has_failures()
    }

    pub(crate) fn failure_summary(&self) -> String {
        let failures = self
            .report
            .results
            .iter()
            .filter(|result| result.outcome != ScenarioOutcome::Passed)
            .map(|result| format!("{}:{:?}", result.spec.name, result.outcome))
            .collect::<Vec<_>>()
            .join(", ");

        let missing_count = self
            .report
            .results
            .iter()
            .filter(|result| result.outcome == ScenarioOutcome::MissingImplementation)
            .count();
        let missing_note = if missing_count > 0 {
            "; missing scenario implementations are phase-gate failures"
        } else {
            ""
        };

        format!(
            "framework-conformance scenario runner failed for {}; failures [{}]",
            self.selector, failures
        ) + missing_note
    }
}

pub(super) fn write_report(dir: &Path, report: &ScenarioReport) -> Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join("report.json");
    std::fs::write(&path, serde_json::to_string_pretty(report)?)?;
    Ok(path)
}
