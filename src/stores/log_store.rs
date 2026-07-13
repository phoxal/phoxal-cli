//! Per-runtime routed log scrollback - ONLY. This is the log-scrollback
//! third of today's `tui::logs::RuntimeLogState`, split out on its own so a
//! consumer that only wants log lines never has to pull in telemetry history
//! or the router's latest traffic sample (see
//! [`crate::stores::telemetry_store::TelemetryStore`] for those).
//!
//! Finding C2: this module used to also carry a second, parallel diagnostics
//! ring (`DiagnosticEvent`/`record_diagnostic`/`diagnostics()`) alongside the
//! log scrollback. It was never wired to anything: the real TUI path renders
//! diagnostics from `tui::startup::StartupState::diagnostics` instead, fed
//! directly from `SessionEvent::Diagnostic` via
//! `StartupState::apply_event`/`tui::render::draw_diagnostics_strip` - a
//! diagnostic never actually reached this store's ring in production. Picking
//! ONE diagnostics buffer (the one the controller/renderer actually uses)
//! means this one goes; a diagnostic's *routing* source/level shape still
//! agrees everywhere because both paths share
//! `crate::session::event::{DiagnosticSource, DiagnosticLevel}`.

use std::collections::{BTreeMap, VecDeque};

use crate::supervisor::{LogSource, RoutedLogLine};

/// Bound on a single runtime's scrollback. Matches the existing
/// `tui::logs::LOG_CAPACITY` value: generous for interactive review without
/// letting a chatty participant grow the TUI's memory unboundedly over a
/// long session.
pub const LOG_CAPACITY: usize = 2000;

/// One rendered log line plus which routing source produced it (design doc:
/// "dedup by ROUTING, not text-compare").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisplayedLine {
    pub source: LogSource,
    pub text: String,
}

/// One runtime's log-only state: its bounded scrollback, plus whether a
/// `LogSource::Bus` line has been observed yet (the routing cutover point).
#[derive(Debug, Clone, Default)]
struct RuntimeLogState {
    lines: VecDeque<DisplayedLine>,
    /// Whether at least one `LogSource::Bus` line has been recorded for this
    /// runtime yet - once true, further `LogSource::Raw` lines are dropped
    /// rather than admitted (see [`LogStore::record`]), since the bus is
    /// assumed to carry everything this participant logs from that point on.
    bus_seen: bool,
}

/// Per-runtime routed log ring buffers + a diagnostics ring. Keyed by
/// participant/runtime id, built from the stream of [`RoutedLogLine`]s the
/// board forwards once a display registers a sink
/// (`BoardBackend::set_log_sink`).
#[derive(Debug, Clone, Default)]
pub struct LogStore {
    runtimes: BTreeMap<String, RuntimeLogState>,
}

impl LogStore {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Route one line. `LogSource::Bus` is always admitted (and marks the
    /// cutover). `LogSource::Raw` is admitted only until the first `Bus`
    /// line for this same id has been seen - after that it is a structural
    /// duplicate (the participant's own stderr mirrors what it now also
    /// publishes on the bus) and is dropped: the whole "dedup by ROUTING,
    /// not text-compare" rule - no string comparison happens at all. This is
    /// the correct, unchanged behavior carried over from the old (now
    /// deleted) `tui::logs::LogRouter::record`.
    pub fn record(&mut self, line: RoutedLogLine) {
        let state = self.runtimes.entry(line.participant).or_default();
        match line.source {
            LogSource::Bus => {
                state.bus_seen = true;
                push_bounded(
                    &mut state.lines,
                    DisplayedLine {
                        source: LogSource::Bus,
                        text: line.text,
                    },
                );
            }
            LogSource::Raw => {
                if state.bus_seen {
                    return;
                }
                push_bounded(
                    &mut state.lines,
                    DisplayedLine {
                        source: LogSource::Raw,
                        text: line.text,
                    },
                );
            }
        }
    }

    /// The full scrollback for `id`, oldest first, as a contiguous slice.
    /// `&mut self` because `VecDeque::make_contiguous` needs it - every call
    /// site (the redraw path) already holds the store mutably, so this costs
    /// nothing in practice and avoids an unconditional per-frame copy.
    #[must_use]
    pub fn lines_for(&mut self, id: &str) -> &[DisplayedLine] {
        self.runtimes
            .get_mut(id)
            .map_or(&[][..], |state| state.lines.make_contiguous())
    }
}

fn push_bounded(lines: &mut VecDeque<DisplayedLine>, line: DisplayedLine) {
    lines.push_back(line);
    if lines.len() > LOG_CAPACITY {
        lines.pop_front();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(id: &str, source: LogSource, text: &str) -> RoutedLogLine {
        RoutedLogLine {
            participant: id.to_string(),
            source,
            text: text.to_string(),
        }
    }

    #[test]
    fn bus_line_after_raw_line_does_not_suppress_the_earlier_raw_line() {
        let mut store = LogStore::new();
        store.record(line("svc", LogSource::Raw, "stderr: pre-setup panic guard"));
        store.record(line("svc", LogSource::Bus, "bus: ready"));
        let lines = store.lines_for("svc");
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].source, LogSource::Raw);
        assert_eq!(lines[1].source, LogSource::Bus);
    }

    #[test]
    fn raw_line_after_a_bus_line_is_dropped_as_a_structural_duplicate() {
        let mut store = LogStore::new();
        store.record(line("svc", LogSource::Bus, "bus: ready"));
        store.record(line("svc", LogSource::Raw, "stderr: ready"));
        let lines = store.lines_for("svc");
        assert_eq!(
            lines.len(),
            1,
            "the raw duplicate must not double-show: {lines:?}"
        );
        assert_eq!(lines[0].source, LogSource::Bus);
    }

    #[test]
    fn bus_and_raw_are_tracked_independently_per_participant() {
        let mut store = LogStore::new();
        store.record(line("a", LogSource::Bus, "a ready"));
        store.record(line("b", LogSource::Raw, "b booting"));
        assert_eq!(store.lines_for("a").len(), 1);
        assert_eq!(store.lines_for("b").len(), 1);
        assert_eq!(store.lines_for("b")[0].source, LogSource::Raw);
    }

    #[test]
    fn unknown_participant_yields_an_empty_slice_not_a_panic() {
        let mut store = LogStore::new();
        assert!(store.lines_for("nope").is_empty());
    }

    #[test]
    fn log_ring_buffer_stays_bounded_at_capacity() {
        let mut store = LogStore::new();
        for i in 0..(LOG_CAPACITY + 50) {
            store.record(line("chatty", LogSource::Bus, &format!("line {i}")));
        }
        assert_eq!(store.lines_for("chatty").len(), LOG_CAPACITY);
        let lines = store.lines_for("chatty");
        assert!(
            lines
                .last()
                .unwrap()
                .text
                .contains(&(LOG_CAPACITY + 49).to_string())
        );
    }
}
