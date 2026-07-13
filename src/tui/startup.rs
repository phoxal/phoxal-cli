//! The pure startup-surface model: which phase (if any) is currently active,
//! a bounded ring of recent diagnostics, and whether the dedicated startup
//! surface (welcome card + active phase + diagnostics) should render at all
//! INSTEAD of the runtime navigator.
//!
//! Deliberately free of ratatui/crossterm: [`TuiDisplay`](super::TuiDisplay)
//! feeds this from [`SessionEvent`]s via [`StartupState::apply_event`], and
//! `tui::render` draws it - this module owns only the reduction, so it is
//! unit-testable without a terminal (see this module's tests).
//!
//! # One current row, not a phase list (Product decision 3)
//!
//! [`StartupState::phase`] holds at most one [`PhaseRow`]: the phase actually
//! in progress, or - once it finishes - a compact completion result shown in
//! the same slot until the NEXT [`SessionEvent::PhaseStarted`] replaces it.
//! Future phases are never pre-created; a phase that never starts (no
//! download, no build) never appears at all.
//!
//! # A one-way ratchet to the runtime navigator
//!
//! [`StartupState::show_startup_surface`] is `true` until the session has
//! EVER reached [`SessionState::Running`] or [`SessionState::Paused`], then
//! stays `false` for the rest of the session - even if a later event moves
//! the session back to [`SessionState::Waiting`] (e.g. the simulation clock
//! going silent mid-session). Without the ratchet, that transition would
//! yank the operator back to the welcome card/phase row mid-session, which
//! is a worse regression than the "waiting" reason simply not being visible
//! in the navigator (a documented follow-up, not this slice's scope).

use std::collections::VecDeque;
use std::time::Duration;

use crate::session::event::{
    DiagnosticLevel, DiagnosticSource, PhaseId, PhaseOutcome, SessionEvent,
};
use crate::session::state::SessionState;

/// Bound on the diagnostics ring: generous enough to show a burst of
/// warnings across every startup stage without ever growing unboundedly.
const DIAGNOSTICS_CAPACITY: usize = 50;

/// One captured diagnostic line, ready for the startup surface's
/// diagnostics area.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DiagnosticLine {
    pub source: DiagnosticSource,
    pub level: DiagnosticLevel,
    pub message: String,
}

/// Progress carried by a [`SessionEvent::PhaseProgress`] for the current
/// phase.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PhaseProgressInfo {
    pub completed: u64,
    pub total: u64,
    pub detail: Option<String>,
}

/// The one current startup row: a phase that has started, optionally with
/// progress, optionally finished (a compact completion result in the same
/// row).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PhaseRow {
    pub id: PhaseId,
    pub label: String,
    pub progress: Option<PhaseProgressInfo>,
    pub outcome: Option<(PhaseOutcome, Duration)>,
}

/// The startup surface's full pure state - see the module docs.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct StartupState {
    pub session_state: SessionState,
    pub phase: Option<PhaseRow>,
    pub diagnostics: VecDeque<DiagnosticLine>,
    reached_running_or_paused: bool,
}

impl StartupState {
    pub(crate) fn new() -> Self {
        Self {
            session_state: SessionState::Preparing,
            phase: None,
            diagnostics: VecDeque::new(),
            reached_running_or_paused: false,
        }
    }

    /// `true` while the dedicated startup surface should render instead of
    /// the runtime navigator - see the module docs for the one-way ratchet.
    #[must_use]
    pub(crate) fn show_startup_surface(&self) -> bool {
        !self.reached_running_or_paused
    }

    /// Fold one [`SessionEvent`] into the model. Free of I/O so it can be
    /// driven directly in a test over a fake event stream.
    pub(crate) fn apply_event(&mut self, event: &SessionEvent) {
        match event {
            SessionEvent::PhaseStarted { id, label } => {
                self.phase = Some(PhaseRow {
                    id: id.clone(),
                    label: label.clone(),
                    progress: None,
                    outcome: None,
                });
            }
            SessionEvent::PhaseProgress {
                id,
                completed,
                total,
                detail,
            } => {
                if let Some(phase) = &mut self.phase
                    && &phase.id == id
                {
                    phase.progress = Some(PhaseProgressInfo {
                        completed: *completed,
                        total: *total,
                        detail: detail.clone(),
                    });
                }
            }
            SessionEvent::PhaseFinished {
                id,
                outcome,
                elapsed,
            } => match &mut self.phase {
                Some(phase) if &phase.id == id => {
                    phase.outcome = Some((outcome.clone(), *elapsed));
                }
                // A `Finished` for an id whose `Started` was never recorded
                // (or a mismatched current phase) still needs to render
                // SOMETHING rather than silently vanishing - fall back to the
                // bare id as its label, mirroring
                // `session::controller::LineRenderer`'s own defensive
                // fallback.
                _ => {
                    self.phase = Some(PhaseRow {
                        id: id.clone(),
                        label: id.to_string(),
                        progress: None,
                        outcome: Some((outcome.clone(), *elapsed)),
                    });
                }
            },
            SessionEvent::Diagnostic {
                source,
                level,
                message,
            } => {
                if self.diagnostics.len() >= DIAGNOSTICS_CAPACITY {
                    self.diagnostics.pop_front();
                }
                self.diagnostics.push_back(DiagnosticLine {
                    source: source.clone(),
                    level: *level,
                    message: message.clone(),
                });
            }
            SessionEvent::SessionChanged { state } => {
                self.session_state = state.clone();
                if matches!(state, SessionState::Running | SessionState::Paused) {
                    self.reached_running_or_paused = true;
                }
            }
            // The controller's own `apply_event` reduces both of these into
            // a `SessionChanged` (via `reduce_clock_observation`/
            // `reduce_staged_startup_complete`) and forwards THAT instead of
            // the raw event - see `session::controller`'s docs. These arms
            // exist only for exhaustiveness; the `SessionChanged` arm above
            // is what this model actually observes.
            SessionEvent::ClockObserved(_) | SessionEvent::StagedStartupComplete => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn started(id: &str, label: &str) -> SessionEvent {
        SessionEvent::PhaseStarted {
            id: PhaseId::new(id),
            label: label.to_string(),
        }
    }

    fn progress(id: &str, completed: u64, total: u64) -> SessionEvent {
        SessionEvent::PhaseProgress {
            id: PhaseId::new(id),
            completed,
            total,
            detail: None,
        }
    }

    fn finished(id: &str, outcome: PhaseOutcome) -> SessionEvent {
        SessionEvent::PhaseFinished {
            id: PhaseId::new(id),
            outcome,
            elapsed: Duration::from_millis(5),
        }
    }

    fn changed(state: SessionState) -> SessionEvent {
        SessionEvent::SessionChanged { state }
    }

    #[test]
    fn a_fresh_state_shows_the_startup_surface_with_no_active_phase() {
        let state = StartupState::new();
        assert!(state.show_startup_surface());
        assert!(state.phase.is_none());
        assert!(state.diagnostics.is_empty());
    }

    /// The exact lifecycle the brief calls out: start -> progress -> finish
    /// -> the next phase replaces it - never a pending list of phases that
    /// have not started yet.
    #[test]
    fn phase_start_progress_finish_then_the_next_phase_replaces_it() {
        let mut state = StartupState::new();

        state.apply_event(&started("download", "Downloading artifacts"));
        let phase = state
            .phase
            .as_ref()
            .expect("a started phase must be current");
        assert_eq!(phase.label, "Downloading artifacts");
        assert!(phase.progress.is_none());
        assert!(phase.outcome.is_none());

        state.apply_event(&progress("download", 3, 10));
        let phase = state.phase.as_ref().unwrap();
        let progress_info = phase.progress.as_ref().expect("progress must be recorded");
        assert_eq!(progress_info.completed, 3);
        assert_eq!(progress_info.total, 10);

        state.apply_event(&finished("download", PhaseOutcome::Succeeded));
        let phase = state.phase.as_ref().unwrap();
        assert_eq!(phase.label, "Downloading artifacts");
        let (outcome, _elapsed) = phase
            .outcome
            .as_ref()
            .expect("a finished phase must record its outcome");
        assert_eq!(*outcome, PhaseOutcome::Succeeded);

        // The next phase replaces the completed row entirely - there is
        // never more than one row, and no trace of "download" lingers.
        state.apply_event(&started("build", "Building participants"));
        let phase = state.phase.as_ref().unwrap();
        assert_eq!(phase.label, "Building participants");
        assert!(phase.outcome.is_none());
        assert!(phase.progress.is_none());
    }

    /// A `Finished` for an id whose `Started` was never observed must not
    /// panic or silently vanish - it becomes its own row with the bare id as
    /// a label, exactly like `LineRenderer`'s own defensive fallback.
    #[test]
    fn a_finish_for_an_unseen_phase_still_renders_a_row() {
        let mut state = StartupState::new();
        state.apply_event(&finished("validate", PhaseOutcome::Skipped));
        let phase = state.phase.as_ref().expect("a fallback row must exist");
        assert_eq!(phase.label, "validate");
        assert_eq!(phase.outcome.as_ref().unwrap().0, PhaseOutcome::Skipped);
    }

    /// Progress for an id that is NOT the current phase must be ignored
    /// rather than corrupting the current row (e.g. a stale/reordered event).
    #[test]
    fn progress_for_a_mismatched_id_is_ignored() {
        let mut state = StartupState::new();
        state.apply_event(&started("download", "Downloading"));
        state.apply_event(&progress("build", 1, 2));
        assert!(state.phase.as_ref().unwrap().progress.is_none());
    }

    #[test]
    fn diagnostics_ring_is_bounded_and_drops_the_oldest_first() {
        let mut state = StartupState::new();
        for index in 0..(DIAGNOSTICS_CAPACITY + 10) {
            state.apply_event(&SessionEvent::Diagnostic {
                source: DiagnosticSource::Cli,
                level: DiagnosticLevel::Info,
                message: format!("line {index}"),
            });
        }
        assert_eq!(state.diagnostics.len(), DIAGNOSTICS_CAPACITY);
        assert_eq!(state.diagnostics.front().unwrap().message, "line 10");
        assert_eq!(
            state.diagnostics.back().unwrap().message,
            format!("line {}", DIAGNOSTICS_CAPACITY + 9)
        );
    }

    /// State -> surface selection: `Preparing`/`Starting`/`Waiting` show the
    /// startup surface; `Running`/`Paused` collapse it - and the collapse is
    /// a one-way ratchet, so a later regression back to `Waiting` (e.g. the
    /// simulation clock going silent) must NOT bring the startup surface
    /// back.
    #[test]
    fn show_startup_surface_ratchets_off_at_running_or_paused_and_never_back_on() {
        let mut state = StartupState::new();
        assert!(state.show_startup_surface());

        state.apply_event(&changed(SessionState::Starting));
        assert!(state.show_startup_surface());

        state.apply_event(&changed(SessionState::Waiting(
            crate::session::state::WaitReason::ClockAbsent,
        )));
        assert!(state.show_startup_surface());

        state.apply_event(&changed(SessionState::Paused));
        assert!(!state.show_startup_surface());

        // Regression back to Waiting must not un-ratchet.
        state.apply_event(&changed(SessionState::Waiting(
            crate::session::state::WaitReason::ClockAbsent,
        )));
        assert!(!state.show_startup_surface());

        state.apply_event(&changed(SessionState::Running));
        assert!(!state.show_startup_surface());
    }

    #[test]
    fn session_changed_updates_the_tracked_state_label() {
        let mut state = StartupState::new();
        state.apply_event(&changed(SessionState::Starting));
        assert_eq!(state.session_state.label(), "starting");
    }
}
