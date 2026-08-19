//! Errors responsibilities for check.

use super::CheckOutcome;
use crate::check as graph_check;
use anyhow::Result;
use anyhow::bail;

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
        graph_check::Problem::InvalidConfig {
            owner: graph_check::ConfigOwner::UserService,
            runtime_id,
            errors,
        } => {
            format!(
                "invalid config for user runtime {runtime_id}: {}",
                errors.join("; ")
            )
        }
        graph_check::Problem::InvalidConfig {
            owner: graph_check::ConfigOwner::ComponentDriver,
            runtime_id,
            errors,
        } => {
            format!(
                "invalid driver config for component instance {runtime_id}: {}",
                errors.join("; ")
            )
        }
        graph_check::Problem::ConnectionKindMismatch {
            instance,
            artifact_id,
            declared,
            authored,
        } => {
            format!(
                "component instance {instance} authors a '{}' connection, but its driver \
                 {artifact_id} accepts only '{}'",
                authored.as_str(),
                declared.as_str(),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::format_problem;
    use crate::check::{ConfigOwner, Problem};
    use phoxal::model::connection::ConnectionKind;

    /// A driven instance is not a user runtime, and the rendered line is the
    /// whole of what an operator gets, so each owner has to name its own
    /// subject.
    #[test]
    fn an_invalid_config_names_the_thing_that_actually_declared_it() {
        assert_eq!(
            format_problem(&Problem::InvalidConfig {
                owner: ConfigOwner::UserService,
                runtime_id: "avoid".to_string(),
                errors: vec!["services.avoid.config: \"gain\" is a required property".to_string()],
            }),
            "invalid config for user runtime avoid: services.avoid.config: \"gain\" is a required \
             property"
        );
        assert_eq!(
            format_problem(&Problem::InvalidConfig {
                owner: ConfigOwner::ComponentDriver,
                runtime_id: "left_drive".to_string(),
                errors: vec![
                    "components.left_drive.driver.config/reduction: \"fast\" is not of type \
                     \"integer\""
                        .to_string()
                ],
            }),
            "invalid driver config for component instance left_drive: \
             components.left_drive.driver.config/reduction: \"fast\" is not of type \"integer\""
        );
    }

    /// The mismatch line has to carry all four facts, because the fix is
    /// either editing that instance's connection or wiring a different driver.
    #[test]
    fn a_connection_mismatch_names_the_instance_driver_and_both_kinds() {
        assert_eq!(
            format_problem(&Problem::ConnectionKindMismatch {
                instance: "left_drive".to_string(),
                artifact_id: "ddsm115".to_string(),
                declared: ConnectionKind::Serial,
                authored: ConnectionKind::Can,
            }),
            "component instance left_drive authors a 'can' connection, but its driver ddsm115 \
             accepts only 'serial'"
        );
    }
}
