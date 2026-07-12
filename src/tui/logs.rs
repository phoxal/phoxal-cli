//! TUI-side bounded log scrollback (Part 3): per-runtime ring buffers fed by
//! [`crate::supervisor::RoutedLogLine`], kept separate from
//! [`crate::supervisor::BoardSnapshot`] (which only keeps ~8 lines/participant
//! for its own persisted-status purposes). Also reserves - empty for now -
//! the per-runtime telemetry time-series (Resources tab) and latest traffic
//! sample (Traffic tab) slots a later slice fills in from the framework's
//! `y2026_9` contracts.

use std::collections::{BTreeMap, VecDeque};

use crate::supervisor::{LogSource, RoutedLogLine};

/// One rendered log line plus which routing source produced it (design doc:
/// "dedup by ROUTING, not text-compare").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisplayedLine {
    pub source: LogSource,
    pub text: String,
}

/// Bound on a single runtime's scrollback. Generous for interactive review
/// without letting a chatty participant grow the TUI's memory unboundedly
/// over a long session.
const LOG_CAPACITY: usize = 2000;

/// One runtime's TUI-side state: its bounded log scrollback, plus reserved
/// (always-empty today) slots for a later slice's telemetry/traffic panels.
///
/// `telemetry_series`/`latest_traffic_sample` are deliberate insertion
/// points (design doc Part 3/4d): nothing in this slice writes or reads them
/// yet - a later slice fills them from the framework's `y2026_9` contracts
/// and renders them on the Resources/Traffic bespoke tabs
/// (`crate::tui::render::draw_bespoke_placeholder`). `#[allow(dead_code)]`
/// documents that this is intentional rather than leftover.
#[derive(Debug, Clone, Default)]
#[allow(dead_code)]
pub struct RuntimeLogState {
    pub lines: VecDeque<DisplayedLine>,
    /// Whether at least one `LogSource::Bus` line has been recorded for this
    /// runtime yet - the routing cutover point: once true, further
    /// `LogSource::Raw` lines are dropped rather than admitted (see
    /// `LogRouter::record`), since the bus is assumed to carry everything
    /// this participant logs from that point on.
    bus_seen: bool,
    /// Reserved for Phase 3c (`y2026_9::process`): CPU%/RAM samples over
    /// time for the Resources tab. Always empty in this slice.
    pub telemetry_series: Vec<()>,
    /// Reserved for the Traffic tab's latest sample. Always `None` in this
    /// slice.
    pub latest_traffic_sample: Option<()>,
}

/// The TUI's full log-routing state: one [`RuntimeLogState`] per participant
/// id, built from the stream of [`RoutedLogLine`]s the board forwards once a
/// display registers a sink (`BoardBackend::set_log_sink`).
#[derive(Debug, Clone, Default)]
pub struct LogRouter {
    runtimes: BTreeMap<String, RuntimeLogState>,
}

impl LogRouter {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Route one line. `LogSource::Bus` is always admitted (and marks the
    /// cutover). `LogSource::Raw` is admitted only until the first `Bus`
    /// line for this same id has been seen - after that it is a structural
    /// duplicate (the participant's own stderr mirrors what it now also
    /// publishes on the bus) and is dropped, which is the whole "dedup by
    /// ROUTING, not text-compare" rule: no string comparison happens at all.
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
    /// site (the redraw path) already holds the router mutably, so this
    /// costs nothing in practice and avoids an unconditional per-frame copy.
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
        let mut router = LogRouter::new();
        router.record(line("svc", LogSource::Raw, "stderr: pre-setup panic guard"));
        router.record(line("svc", LogSource::Bus, "bus: ready"));
        let lines = router.lines_for("svc");
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].source, LogSource::Raw);
        assert_eq!(lines[1].source, LogSource::Bus);
    }

    #[test]
    fn raw_line_after_a_bus_line_is_dropped_as_a_structural_duplicate() {
        let mut router = LogRouter::new();
        router.record(line("svc", LogSource::Bus, "bus: ready"));
        router.record(line("svc", LogSource::Raw, "stderr: ready"));
        let lines = router.lines_for("svc");
        assert_eq!(
            lines.len(),
            1,
            "the raw duplicate must not double-show: {lines:?}"
        );
        assert_eq!(lines[0].source, LogSource::Bus);
    }

    #[test]
    fn bus_and_raw_are_tracked_independently_per_participant() {
        let mut router = LogRouter::new();
        router.record(line("a", LogSource::Bus, "a ready"));
        router.record(line("b", LogSource::Raw, "b booting"));
        assert_eq!(router.lines_for("a").len(), 1);
        assert_eq!(router.lines_for("b").len(), 1);
        assert_eq!(router.lines_for("b")[0].source, LogSource::Raw);
    }

    #[test]
    fn unknown_participant_yields_an_empty_slice_not_a_panic() {
        let mut router = LogRouter::new();
        assert!(router.lines_for("nope").is_empty());
    }

    #[test]
    fn ring_buffer_stays_bounded_at_capacity() {
        let mut router = LogRouter::new();
        for i in 0..(LOG_CAPACITY + 50) {
            router.record(line("chatty", LogSource::Bus, &format!("line {i}")));
        }
        assert_eq!(router.lines_for("chatty").len(), LOG_CAPACITY);
        // The oldest lines must have been evicted, newest retained.
        let lines = router.lines_for("chatty");
        assert!(
            lines
                .last()
                .unwrap()
                .text
                .contains(&(LOG_CAPACITY + 49).to_string())
        );
    }
}
