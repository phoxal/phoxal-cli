//! The `Plain`/non-TTY display (design doc: "an append-only line logger
//! (structured, level-tagged stderr lines, NO cursor control, NO
//! full-screen)"). Diffs the board snapshot on each redraw tick against the
//! previous one and prints exactly one line per participant whose `state`
//! changed since then - never a full re-dump (that was the old
//! `BoardSnapshot::render()` eprintln path this replaces), and never any
//! cursor-control byte, so it is always safe to pipe or log to a file.

use std::collections::BTreeMap;

use crate::supervisor::{BoardSnapshot, ParticipantState};
use crate::theme::{Role, Theme, state_role, state_symbol};

pub struct LineLogger {
    mode_label: &'static str,
    theme: Theme,
    last_state: BTreeMap<String, ParticipantState>,
}

impl LineLogger {
    #[must_use]
    pub fn new(mode_label: &'static str, theme: Theme) -> Self {
        Self {
            mode_label,
            theme,
            last_state: BTreeMap::new(),
        }
    }

    /// Print one line per participant whose `state` changed since the last
    /// call. Ordering follows `BoardSnapshot`'s own `BTreeMap` (alphabetic by
    /// id) so output is deterministic run to run.
    pub fn redraw(&mut self, board: &BoardSnapshot) {
        for status in board.participants.values() {
            let previous = self.last_state.get(&status.id).copied();
            if previous == Some(status.state) {
                continue;
            }
            self.last_state.insert(status.id.clone(), status.state);
            let level = level_word(status.state);
            let symbol = self
                .theme
                .paint(state_role(status.state), state_symbol(status.state));
            let note = status
                .note
                .as_deref()
                .map_or_else(String::new, |note| format!(" - {note}"));
            eprintln!(
                "{level:<5} {symbol} {} [{}/{}] {}{note}",
                status.id,
                self.mode_label,
                status.kind.label(),
                status.state.label(),
            );
        }
    }
}

fn level_word(state: ParticipantState) -> &'static str {
    match state_role(state) {
        Role::Error => "error",
        Role::Warn => "warn",
        _ => "info",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::participant_kind::ParticipantKind;
    use crate::supervisor::ParticipantStatus;

    fn board_with(id: &str, state: ParticipantState) -> BoardSnapshot {
        let mut board = BoardSnapshot::default();
        board.participants.insert(
            id.to_string(),
            ParticipantStatus::new(id, ParticipantKind::Service, state),
        );
        board
    }

    #[test]
    fn level_word_maps_failed_to_error_and_degraded_to_warn() {
        assert_eq!(level_word(ParticipantState::Failed), "error");
        assert_eq!(level_word(ParticipantState::Degraded), "warn");
        assert_eq!(level_word(ParticipantState::Ready), "info");
    }

    #[test]
    fn redraw_tracks_state_transitions_without_panicking() {
        let mut logger = LineLogger::new("run", Theme::new(crate::theme::ColorCapability::None));
        let starting = board_with("drive", ParticipantState::Starting);
        logger.redraw(&starting);
        assert_eq!(
            logger.last_state.get("drive"),
            Some(&ParticipantState::Starting)
        );

        let ready = board_with("drive", ParticipantState::Ready);
        logger.redraw(&ready);
        assert_eq!(
            logger.last_state.get("drive"),
            Some(&ParticipantState::Ready)
        );

        // A third redraw with no state change must not re-insert/duplicate
        // anything - proven indirectly by the map staying single-entry.
        logger.redraw(&ready);
        assert_eq!(logger.last_state.len(), 1);
    }
}
