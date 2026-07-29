//! Ephemeral terminal projection assembled at each redraw. It borrows the
//! durable board and session-only stores; it is never serialized or persisted.

use std::cmp::Ordering;
use std::time::Instant;

use crate::tui::log_view::LogView;
use crate::tui::runtime_view::RuntimeView;
use crate::tui::visibility::is_visible_runtime;
use phoxal_cli_core::session::ParticipantKind;
use phoxal_cli_core::session::RuntimePerformanceSample;
use phoxal_cli_core::session::TelemetrySnapshot;
use phoxal_cli_core::session::Timestamped;
use phoxal_cli_core::session::{BoardSnapshot, ParticipantState, ParticipantStatus};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RuntimeSummary {
    pub ready: usize,
    pub degraded: usize,
    pub failed: usize,
    pub starting: usize,
    pub restarts: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum RuntimeGroup {
    UserServices,
    FrameworkServices,
    Drivers,
}

impl RuntimeGroup {
    pub const ALL: [Self; 3] = [Self::UserServices, Self::FrameworkServices, Self::Drivers];

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::UserServices => "User services",
            Self::FrameworkServices => "Framework services",
            Self::Drivers => "Drivers",
        }
    }
}

pub struct SessionViewModel<'a> {
    pub board: &'a BoardSnapshot,
    pub logs: &'a LogView,
    pub runtime: &'a RuntimeView,
    pub telemetry: &'a TelemetrySnapshot,
    pub now: Instant,
    pub runtimes: Vec<&'a ParticipantStatus>,
    pub summary: RuntimeSummary,
}

impl<'a> SessionViewModel<'a> {
    #[must_use]
    pub fn runtime_performance(
        &self,
        status: &ParticipantStatus,
    ) -> Option<&Timestamped<RuntimePerformanceSample>> {
        status
            .scope
            .as_ref()
            .and_then(|scope| self.telemetry.runtime(scope, &status.id))
    }

    #[must_use]
    pub fn runtime_topic_count(&self, participant_id: &str) -> usize {
        self.runtimes
            .iter()
            .find(|status| status.id == participant_id)
            .filter(|status| status.present != Some(false))
            .and_then(|status| self.runtime_performance(status))
            .filter(|sample| {
                !sample.is_stale(self.now, phoxal_cli_core::session::DEFAULT_FRESHNESS_TTL)
            })
            .map_or(0, |sample| {
                sample.value.topics.len() + usize::from(sample.value.overflow.is_some())
            })
    }

    #[must_use]
    pub fn new(
        board: &'a BoardSnapshot,
        logs: &'a LogView,
        runtime: &'a RuntimeView,
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
            summary.restarts = summary.restarts.saturating_add(status.restart_count);
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
                ) || status.restart_count > 0
            })
            .collect()
    }

    #[must_use]
    pub fn summary_for_mode(&self, simulation: bool) -> RuntimeSummary {
        if !simulation {
            return self.summary;
        }
        let mut summary = RuntimeSummary::default();
        for status in self
            .runtimes
            .iter()
            .copied()
            .filter(|status| self.runtime_is_loaded(status, simulation))
        {
            match status.state {
                ParticipantState::Ready => summary.ready += 1,
                ParticipantState::Degraded => summary.degraded += 1,
                ParticipantState::Failed => summary.failed += 1,
                ParticipantState::Starting | ParticipantState::Restarting => summary.starting += 1,
                ParticipantState::Stopped => {}
            }
            summary.restarts = summary.restarts.saturating_add(status.restart_count);
        }
        summary
    }

    #[must_use]
    pub fn needs_attention_for_mode(&self, simulation: bool) -> Vec<&ParticipantStatus> {
        self.needs_attention()
            .into_iter()
            .filter(|status| self.runtime_is_loaded(status, simulation))
            .collect()
    }

    #[must_use]
    pub fn runtime_is_loaded(&self, status: &ParticipantStatus, simulation: bool) -> bool {
        !simulation || self.runtime_group(status) != RuntimeGroup::Drivers
    }

    #[must_use]
    pub fn runtime_group(&self, status: &ParticipantStatus) -> RuntimeGroup {
        if status.kind == ParticipantKind::Driver {
            return RuntimeGroup::Drivers;
        }
        if self.runtime.is_user_service(status) {
            RuntimeGroup::UserServices
        } else {
            RuntimeGroup::FrameworkServices
        }
    }

    #[must_use]
    pub fn runtimes_in(&self, group: RuntimeGroup) -> Vec<(usize, &'a ParticipantStatus)> {
        self.runtimes
            .iter()
            .copied()
            .enumerate()
            .filter(|(_, status)| self.runtime_group(status) == group)
            .collect()
    }

    #[must_use]
    pub fn runtimes_in_mode(
        &self,
        group: RuntimeGroup,
        simulation: bool,
    ) -> Vec<(usize, &'a ParticipantStatus)> {
        self.runtimes_in(group)
            .into_iter()
            .filter(|(_, status)| self.runtime_is_loaded(status, simulation))
            .collect()
    }
}

fn runtime_order(
    left: &ParticipantStatus,
    right: &ParticipantStatus,
    runtime: &RuntimeView,
) -> Ordering {
    let group = |status: &ParticipantStatus| {
        if status.kind == ParticipantKind::Driver {
            RuntimeGroup::Drivers
        } else if runtime.is_user_service(status) {
            RuntimeGroup::UserServices
        } else {
            RuntimeGroup::FrameworkServices
        }
    };
    let priority = |status: &ParticipantStatus| match status.state {
        ParticipantState::Failed => 0,
        ParticipantState::Degraded => 1,
        ParticipantState::Starting | ParticipantState::Restarting => 2,
        _ if status.restart_count > 0 => 3,
        _ => 4,
    };
    group(left).cmp(&group(right)).then_with(|| {
        priority(left)
            .cmp(&priority(right))
            .then_with(|| left.id.cmp(&right.id))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use phoxal_cli_core::session::ParticipantKind;

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
        let logs = LogView::new();
        let runtime = RuntimeView::new();
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

    #[test]
    fn runtime_rows_group_user_framework_then_drivers() {
        let mut board = BoardSnapshot::default();
        for (id, kind) in [
            ("framework", ParticipantKind::Service),
            ("driver", ParticipantKind::Driver),
            ("user", ParticipantKind::Service),
        ] {
            board.participants.insert(
                id.to_string(),
                ParticipantStatus::new(id, kind, ParticipantState::Ready),
            );
        }
        let logs = LogView::new();
        board
            .participants
            .get_mut("user")
            .unwrap()
            .runtime_user_service = true;
        let runtime = RuntimeView::new();
        let telemetry = TelemetrySnapshot::default();
        let model = SessionViewModel::new(&board, &logs, &runtime, &telemetry, Instant::now());
        assert_eq!(
            model
                .runtimes
                .iter()
                .map(|status| status.id.as_str())
                .collect::<Vec<_>>(),
            ["user", "framework", "driver"]
        );
    }
}
