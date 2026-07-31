//! Typed progress emitted by project preparation without owning presentation.

use std::time::{Duration, Instant};

pub use phoxal_cli_core::runtime::{PhaseId, PhaseOutcome};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreparationEvent {
    Info(String),
    Success(String),
    Warn(String),
    Error(String),
    CommandLine(String),
    PhaseStarted {
        id: PhaseId,
        label: String,
    },
    PhaseFinished {
        id: PhaseId,
        outcome: PhaseOutcome,
        elapsed: Duration,
    },
    ProjectResolved {
        train: String,
    },
}

pub trait Reporter: Send + Sync {
    fn report(&self, event: PreparationEvent);

    fn is_cancelled(&self) -> bool {
        false
    }

    fn info(&self, message: String) {
        self.report(PreparationEvent::Info(message));
    }

    fn success(&self, message: String) {
        self.report(PreparationEvent::Success(message));
    }

    fn warn(&self, message: String) {
        self.report(PreparationEvent::Warn(message));
    }

    fn error(&self, message: String) {
        self.report(PreparationEvent::Error(message));
    }
}

#[derive(Debug, Default)]
pub struct SilentReporter;

impl Reporter for SilentReporter {
    fn report(&self, _event: PreparationEvent) {}
}

pub(crate) fn run_phase<T>(
    reporter: &dyn Reporter,
    id: impl Into<PhaseId>,
    label: impl Into<String>,
    work: impl FnOnce() -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    let id = id.into();
    reporter.report(PreparationEvent::PhaseStarted {
        id: id.clone(),
        label: label.into(),
    });
    let started = Instant::now();
    let result = work();
    let outcome = match &result {
        Ok(_) => PhaseOutcome::Succeeded,
        Err(error) => PhaseOutcome::Failed {
            error: format!("{error:#}"),
        },
    };
    reporter.report(PreparationEvent::PhaseFinished {
        id,
        outcome,
        elapsed: started.elapsed(),
    });
    result
}

pub(crate) fn ensure_active(reporter: &dyn Reporter) -> anyhow::Result<()> {
    anyhow::ensure!(!reporter.is_cancelled(), "project preparation cancelled");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct RecordingReporter(Mutex<Vec<PreparationEvent>>);

    impl Reporter for RecordingReporter {
        fn report(&self, event: PreparationEvent) {
            self.0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(event);
        }
    }

    #[test]
    fn run_phase_reports_real_start_and_terminal_outcome() {
        let reporter = RecordingReporter::default();
        let result: anyhow::Result<()> =
            run_phase(&reporter, PhaseId::new("fixture"), "Fixture phase", || {
                anyhow::bail!("fixture failed")
            });
        assert!(result.is_err());
        let events = reporter
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(matches!(
            &events[0],
            PreparationEvent::PhaseStarted { id, label }
                if id.to_string() == "fixture" && label == "Fixture phase"
        ));
        assert!(matches!(
            &events[1],
            PreparationEvent::PhaseFinished {
                id,
                outcome: PhaseOutcome::Failed { error },
                ..
            } if id.to_string() == "fixture" && error.contains("fixture failed")
        ));
    }
}
