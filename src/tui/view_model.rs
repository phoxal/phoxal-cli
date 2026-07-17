//! Ephemeral session projection assembled at each redraw. It borrows the
//! durable board and session-only stores; it is never serialized or persisted.

use std::cmp::Ordering;
use std::time::Instant;

use crate::stores::log_store::LogStore;
use crate::stores::runtime_store::RuntimeStore;
use crate::supervisor::{BoardSnapshot, ParticipantState, ParticipantStatus};
use crate::telemetry::TelemetrySnapshot;
use crate::tui::visibility::is_visible_runtime;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RuntimeSummary {
    pub ready: usize,
    pub degraded: usize,
    pub failed: usize,
    pub starting: usize,
    pub restarts: u32,
}

pub struct SessionViewModel<'a> {
    pub board: &'a BoardSnapshot,
    pub logs: &'a LogStore,
    pub runtime: &'a RuntimeStore,
    pub telemetry: &'a TelemetrySnapshot,
    pub now: Instant,
    pub runtimes: Vec<&'a ParticipantStatus>,
    pub summary: RuntimeSummary,
}

impl<'a> SessionViewModel<'a> {
    #[must_use]
    pub fn new(
        board: &'a BoardSnapshot,
        logs: &'a LogStore,
        runtime: &'a RuntimeStore,
        telemetry: &'a TelemetrySnapshot,
        now: Instant,
    ) -> Self {
        let mut runtimes = board
            .participants
            .values()
            .filter(|status| is_visible_runtime(status, runtime))
            .collect::<Vec<_>>();
        runtimes.sort_by(|left, right| runtime_order(left, right, runtime));
        let mut summary = RuntimeSummary::default();
        for status in &runtimes {
            match status.state {
                ParticipantState::Ready => summary.ready += 1,
                ParticipantState::Degraded => summary.degraded += 1,
                ParticipantState::Failed => summary.failed += 1,
                ParticipantState::Starting | ParticipantState::Restarting => {
                    summary.starting += 1;
                }
                ParticipantState::Stopped => {}
            }
            summary.restarts = summary.restarts.saturating_add(
                runtime
                    .observation(&status.id)
                    .map_or(status.restart_count, |observation| {
                        observation.displayed_restarts()
                    }),
            );
        }
        Self {
            board,
            logs,
            runtime,
            telemetry,
            now,
            runtimes,
            summary,
        }
    }

    #[must_use]
    pub fn needs_attention(&self) -> Vec<&ParticipantStatus> {
        self.runtimes
            .iter()
            .copied()
            .filter(|status| {
                matches!(
                    status.state,
                    ParticipantState::Failed
                        | ParticipantState::Degraded
                        | ParticipantState::Starting
                        | ParticipantState::Restarting
                ) || self
                    .runtime
                    .observation(&status.id)
                    .is_some_and(|observation| observation.displayed_restarts() > 0)
            })
            .collect()
    }
}

fn runtime_order(
    left: &ParticipantStatus,
    right: &ParticipantStatus,
    runtime: &RuntimeStore,
) -> Ordering {
    let priority = |status: &ParticipantStatus| match status.state {
        ParticipantState::Failed => 0,
        ParticipantState::Degraded => 1,
        ParticipantState::Starting | ParticipantState::Restarting => 2,
        _ if runtime
            .observation(&status.id)
            .is_some_and(|observation| observation.displayed_restarts() > 0) =>
        {
            3
        }
        _ => 4,
    };
    priority(left)
        .cmp(&priority(right))
        .then_with(|| left.id.cmp(&right.id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::participant_kind::ParticipantKind;

    #[test]
    fn runtime_rows_share_the_locked_attention_order() {
        let mut board = BoardSnapshot::default();
        for (id, state) in [
            ("ready", ParticipantState::Ready),
            ("starting", ParticipantState::Starting),
            ("failed", ParticipantState::Failed),
            ("degraded", ParticipantState::Degraded),
        ] {
            board.participants.insert(
                id.to_string(),
                ParticipantStatus::new(id, ParticipantKind::Service, state),
            );
        }
        let logs = LogStore::new();
        let runtime = RuntimeStore::new();
        let telemetry = TelemetrySnapshot::default();
        let model = SessionViewModel::new(&board, &logs, &runtime, &telemetry, Instant::now());
        assert_eq!(
            model
                .runtimes
                .iter()
                .map(|status| status.id.as_str())
                .collect::<Vec<_>>(),
            ["failed", "degraded", "starting", "ready"]
        );
    }
}
