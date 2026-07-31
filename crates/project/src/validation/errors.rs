//! Errors responsibilities for check.

use super::CheckOutcome;
use anyhow::Result;
use anyhow::bail;
use phoxal_cli_core::check as graph_check;

pub(crate) fn ensure_check_outcome_ok(outcome: &CheckOutcome) -> Result<()> {
    if !outcome.report.is_ok() {
        bail!("{}", format_report_error(&outcome.report));
    }

    Ok(())
}

pub(super) fn format_report_error(report: &graph_check::Report) -> String {
    let mut message = String::from("robot graph check failed:");
    for problem in &report.problems {
        message.push_str("\n  - ");
        message.push_str(&format_problem(problem));
    }
    message
}

pub(super) fn format_problem(problem: &graph_check::Problem) -> String {
    match problem {
        graph_check::Problem::InvalidConfig { runtime_id, errors } => {
            format!(
                "invalid config for user runtime {runtime_id}: {}",
                errors.join("; ")
            )
        }
    }
}
