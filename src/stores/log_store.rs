//! Per-runtime routed log scrollback + diagnostic events - ONLY. This is the
//! log-scrollback third of today's `tui::logs::RuntimeLogState`, split out
//! on its own so a consumer that only wants log lines never has to pull in
//! telemetry history or the router's latest traffic sample (see
//! [`crate::stores::telemetry_store::TelemetryStore`] for those).
//!
//! Diagnostic events (tracing records, captured dependency/tool/supervisor
//! output) are a separate channel from routed participant log lines: they
//! are not attributed to a bus participant id the way a [`RoutedLogLine`]
//! is, so they get their own bounded ring rather than being folded into a
//! per-runtime scrollback.
//!
//! NOTE: a parallel slice owns `crate::session::event`, which defines a
//! richer `DiagnosticSource` (an enum: `Tracing`/`Dependency`/`Tool { name }`/
//! `Supervisor`) plus its own `DiagnosticLevel`, intended to become the
//! session-wide typed event stream a later `SessionController` drives. Its
//! shape does not match this module's `DiagnosticEvent` one-for-one (a plain
//! `String` source label here vs. a closed enum there), and this slice is
//! scoped to NOT touch `src/session/` - so [`DiagnosticEvent`] below is a
//! deliberately minimal, local stand-in (source label, level, message)
//! rather than a reuse. Reconcile the two in a later slice, most likely by
//! having `LogStore::record_diagnostic` accept an adapted
//! `session::event::SessionEvent::Diagnostic` payload instead of this local
//! type.

use std::collections::{BTreeMap, VecDeque};

use crate::supervisor::{LogSource, RoutedLogLine};

/// Bound on a single runtime's scrollback. Matches the existing
/// `tui::logs::LOG_CAPACITY` value: generous for interactive review without
/// letting a chatty participant grow the TUI's memory unboundedly over a
/// long session.
pub const LOG_CAPACITY: usize = 2000;

/// Bound on the diagnostics ring. Diagnostics are lower-volume than routed
/// log lines (tracing records, not per-line participant stdout/stderr), so a
/// smaller cap than [`LOG_CAPACITY`] is enough to cover a long interactive
/// session's worth of warnings/errors without growing unboundedly.
pub const DIAGNOSTIC_CAPACITY: usize = 500;

/// One rendered log line plus which routing source produced it (design doc:
/// "dedup by ROUTING, not text-compare").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisplayedLine {
    pub source: LogSource,
    pub text: String,
}

/// Severity of a [`DiagnosticEvent`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum DiagnosticLevel {
    Info,
    Warn,
    Error,
}

/// One diagnostic event: a tracing record or captured non-bus output,
/// attributed to a short source label (a tool name, "supervisor", "tracing",
/// ...) rather than a bus participant id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiagnosticEvent {
    pub source: String,
    pub level: DiagnosticLevel,
    pub message: String,
}

impl DiagnosticEvent {
    #[must_use]
    pub fn new(
        source: impl Into<String>,
        level: DiagnosticLevel,
        message: impl Into<String>,
    ) -> Self {
        Self {
            source: source.into(),
            level,
            message: message.into(),
        }
    }
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
    diagnostics: VecDeque<DiagnosticEvent>,
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
    /// the correct, unchanged behavior carried over from
    /// `tui::logs::LogRouter::record`.
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

    /// Record one diagnostic event into the bounded ring.
    pub fn record_diagnostic(&mut self, event: DiagnosticEvent) {
        self.diagnostics.push_back(event);
        if self.diagnostics.len() > DIAGNOSTIC_CAPACITY {
            self.diagnostics.pop_front();
        }
    }

    /// The full diagnostics ring, oldest first, as a contiguous slice.
    #[must_use]
    pub fn diagnostics(&mut self) -> &[DiagnosticEvent] {
        self.diagnostics.make_contiguous()
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

    #[test]
    fn diagnostics_ring_is_bounded_and_ordered() {
        let mut store = LogStore::new();
        for i in 0..(DIAGNOSTIC_CAPACITY + 10) {
            store.record_diagnostic(DiagnosticEvent::new(
                "tool-router",
                DiagnosticLevel::Info,
                format!("diagnostic {i}"),
            ));
        }
        let diagnostics = store.diagnostics();
        assert_eq!(diagnostics.len(), DIAGNOSTIC_CAPACITY);
        // Oldest evicted, newest retained, and order preserved (oldest
        // surviving entry first).
        assert!(diagnostics[0].message.contains("diagnostic 10"));
        assert!(
            diagnostics
                .last()
                .unwrap()
                .message
                .contains(&(DIAGNOSTIC_CAPACITY + 9).to_string())
        );
    }

    #[test]
    fn diagnostics_are_kept_separate_per_level() {
        let mut store = LogStore::new();
        store.record_diagnostic(DiagnosticEvent::new(
            "tracing",
            DiagnosticLevel::Warn,
            "router queue backing up",
        ));
        store.record_diagnostic(DiagnosticEvent::new(
            "supervisor",
            DiagnosticLevel::Error,
            "participant crashed",
        ));
        let diagnostics = store.diagnostics();
        assert_eq!(diagnostics.len(), 2);
        assert_eq!(diagnostics[0].level, DiagnosticLevel::Warn);
        assert_eq!(diagnostics[1].level, DiagnosticLevel::Error);
    }
}
