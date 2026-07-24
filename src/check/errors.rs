//! Errors responsibilities for check.

use super::CheckOutcome;
use anyhow::Result;
use anyhow::bail;
use phoxal::check as graph_check;
use std::fmt;

pub(crate) fn ensure_check_outcome_ok(train: &str, outcome: &CheckOutcome) -> Result<()> {
    if !outcome.missing_images.is_empty() {
        bail!(
            "{}",
            format_missing_images_error(train, &outcome.missing_images)
        );
    }

    if !outcome.report.is_ok() {
        bail!("{}", format_report_error(&outcome.report));
    }

    Ok(())
}

pub(super) fn format_missing_images_error(train: &str, missing_images: &[String]) -> String {
    let mut message =
        format!("required official artifacts are not available in framework train {train}");
    message.push_str("\n\nMissing official artifacts:");
    for image_ref in missing_images {
        message.push_str("\n  - ");
        message.push_str(image_ref);
    }
    message.push_str("\n\nFix:");
    message.push_str(
        "\n  - refresh or override the generated artifact suite with `phoxal --suite <path> check`",
    );
    message.push_str("\n  - or use a framework train that publishes the required target artifacts");
    message
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
                "invalid config for user service {runtime_id}: {}",
                errors.join("; ")
            )
        }
    }
}

#[derive(Debug)]
pub struct MissingImageError {
    source: anyhow::Error,
}

impl MissingImageError {
    pub fn new(source: anyhow::Error) -> Self {
        Self { source }
    }
}

impl fmt::Display for MissingImageError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("official artifact could not be obtained")
    }
}

impl std::error::Error for MissingImageError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.source.as_ref())
    }
}
