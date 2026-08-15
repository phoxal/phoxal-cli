//! The one block a session ends on.
//!
//! A dashboard session leaves the alternate screen with its last frame gone and
//! whatever the transport said on the way out - none of which tells an operator
//! why the session is over. This is the plain answer, printed once, after the
//! terminal has been given back and before the process exits.

use std::path::{Path, PathBuf};

use phoxal_cli_ui::{AttachmentOutcome, Theme};

/// How a session ended, plus where to read what happened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SessionSummary {
    ending: String,
    logs: Vec<PathBuf>,
}

impl SessionSummary {
    pub(crate) fn new(ending: impl Into<String>, logs: Vec<PathBuf>) -> Self {
        Self {
            ending: ending.into(),
            logs,
        }
    }

    /// The block, relative to `project` so the paths stay readable.
    pub(crate) fn lines(&self, project: &Path) -> Vec<String> {
        let mut lines = vec![format!(
            "  session ended: {}",
            phoxal_cli_observation::sanitize_terminal_text(&self.ending)
        )];
        for (index, log) in self.logs.iter().filter(|log| log.exists()).enumerate() {
            let label = if index == 0 { "logs" } else { "    " };
            let path = log.strip_prefix(project).unwrap_or(log);
            lines.push(format!("  {label} {}", path.display()));
        }
        lines
    }

    /// Print the block on stderr. The dashboard is already gone by here: this
    /// is the only thing between the last frame and the shell prompt.
    pub(crate) fn print(&self, project: &Path, theme: Theme) {
        eprintln!();
        for line in self.lines(project) {
            eprintln!("{}", theme.steel(&line));
        }
    }
}

/// How an attachment ended, in the operator's terms.
///
/// A transport that lost its supervisor identity is deliberately not quoted
/// here: after a stop that loss is the expected consequence, not the cause, and
/// naming it would answer a question nobody asked.
pub(crate) fn attachment_ending(outcome: &AttachmentOutcome) -> String {
    match outcome {
        AttachmentOutcome::Detached => "you detached; the execution keeps running".to_string(),
        AttachmentOutcome::ExecutionStopped => "the execution stopped".to_string(),
        AttachmentOutcome::ExecutionFailed {
            reason: Some(failure),
        } => format!(
            "the execution failed: {:?}: {}",
            failure.reason,
            failure.detail.as_str()
        ),
        AttachmentOutcome::ExecutionFailed { reason: None } => {
            "the execution ended without reporting a reason".to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use phoxal_client::supervisor::execution::{
        Detail, SupervisorFailure, SupervisorFailureReason,
    };

    /// Every ending says something an operator can act on, and none of them
    /// blames the transport for a stop the operator asked for.
    #[test]
    fn every_attachment_ending_is_explained_without_transport_noise() {
        assert!(attachment_ending(&AttachmentOutcome::Detached).contains("keeps running"));
        assert_eq!(
            attachment_ending(&AttachmentOutcome::ExecutionStopped),
            "the execution stopped"
        );
        assert_eq!(
            attachment_ending(&AttachmentOutcome::ExecutionFailed { reason: None }),
            "the execution ended without reporting a reason"
        );
        assert!(
            !attachment_ending(&AttachmentOutcome::ExecutionFailed { reason: None })
                .contains("identity")
        );
        assert_eq!(
            attachment_ending(&AttachmentOutcome::ExecutionFailed {
                reason: Some(SupervisorFailure {
                    reason: SupervisorFailureReason::LaunchFailed,
                    detail: Detail::new("drive never became ready"),
                }),
            }),
            "the execution failed: LaunchFailed: drive never became ready"
        );
    }

    /// The block names each log once, relative to the project, and skips one
    /// that was never written.
    #[test]
    fn the_block_lists_only_the_logs_that_exist() {
        let directory = tempfile::tempdir().unwrap();
        let project = directory.path();
        let supervisor = project.join(".phoxal/run/supervisor.log");
        std::fs::create_dir_all(supervisor.parent().unwrap()).unwrap();
        std::fs::write(&supervisor, "started\n").unwrap();
        let webots = project.join(".phoxal/run/webots.log");

        let lines =
            SessionSummary::new("the execution stopped", vec![supervisor, webots]).lines(project);
        assert_eq!(
            lines,
            vec![
                "  session ended: the execution stopped".to_string(),
                "  logs .phoxal/run/supervisor.log".to_string(),
            ]
        );
    }
}
