//! TUI-side bounded log scrollback (Part 3): per-runtime ring buffers fed by
//! [`crate::supervisor::RoutedLogLine`], kept separate from
//! [`crate::supervisor::BoardSnapshot`] (which only keeps ~8 lines/participant
//! for its own persisted-status purposes). Also reserves - empty for now -
//! the per-runtime telemetry time-series (Resources tab) and latest traffic
//! sample (Traffic tab) slots a later slice fills in from the framework's
//! `y2026_9` contracts.

use std::collections::{BTreeMap, VecDeque};

use crate::launch_plan::{SITE_TOOL_ROUTER, SITE_TOOL_TELEMETRY};
use crate::supervisor::{LogSource, RoutedLogLine};
use crate::telemetry::{RouterMetricsSample, TelemetrySnapshot};

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

/// Bound on the Resources tab's rolling host-sample history: at the ~1s
/// `tool-telemetry` sample cadence, 120 points covers a 2-minute window -
/// enough for a sparkline to read as a trend without growing unboundedly
/// over a long session.
const TELEMETRY_SERIES_CAPACITY: usize = 120;

/// One `telemetry::Host` sample recorded into a Resources tab's rolling
/// history (`RuntimeLogState::telemetry_series`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HostPoint {
    pub cpu_pct: f32,
    pub ram_used_bytes: u64,
    pub ram_total_bytes: u64,
}

/// One runtime's TUI-side state: its bounded log scrollback, plus the
/// telemetry/traffic panel slots the design doc reserved (Part 3/4d) - a
/// rolling `telemetry_series` history for the `tool-telemetry` Resources tab,
/// fed by `LogRouter::record_telemetry` off `telemetry::Host` samples, and
/// `latest_traffic_sample` for the `tool-router` Traffic tab, holding the
/// router's own most recent `router::Metrics` snapshot (a "latest", not a
/// series, since the Traffic table itself already renders the full topic
/// breakdown from one sample).
#[derive(Debug, Clone, Default)]
pub struct RuntimeLogState {
    pub lines: VecDeque<DisplayedLine>,
    /// Whether at least one `LogSource::Bus` line has been recorded for this
    /// runtime yet - the routing cutover point: once true, further
    /// `LogSource::Raw` lines are dropped rather than admitted (see
    /// `LogRouter::record`), since the bus is assumed to carry everything
    /// this participant logs from that point on.
    bus_seen: bool,
    pub telemetry_series: Vec<HostPoint>,
    pub latest_traffic_sample: Option<RouterMetricsSample>,
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

    /// Fold one telemetry snapshot into the reserved panel slots (called once
    /// per redraw, right after `record`ing this tick's log lines): pushes a
    /// `HostPoint` onto `tool-telemetry`'s rolling history when a NEW `Host`
    /// sample has arrived (deduped against the last pushed point, so a
    /// redraw tick that observes no new sample - the common case at a 500ms
    /// tick against a ~1s publish cadence - does not flatten the series with
    /// repeats), and overwrites `tool-router`'s latest traffic sample
    /// unconditionally.
    pub fn record_telemetry(&mut self, telemetry: &TelemetrySnapshot) {
        if let Some(host) = telemetry.host {
            let point = HostPoint {
                cpu_pct: host.cpu_pct,
                ram_used_bytes: host.ram_used_bytes,
                ram_total_bytes: host.ram_total_bytes,
            };
            let state = self
                .runtimes
                .entry(SITE_TOOL_TELEMETRY.to_string())
                .or_default();
            if state.telemetry_series.last() != Some(&point) {
                state.telemetry_series.push(point);
                if state.telemetry_series.len() > TELEMETRY_SERIES_CAPACITY {
                    state.telemetry_series.remove(0);
                }
            }
        }
        if let Some(router) = &telemetry.router {
            self.runtimes
                .entry(SITE_TOOL_ROUTER.to_string())
                .or_default()
                .latest_traffic_sample = Some(router.clone());
        }
    }

    /// The rolling host-sample history for `id`'s Resources tab (empty until
    /// the first `Host` sample has been recorded).
    #[must_use]
    pub fn telemetry_series_for(&self, id: &str) -> &[HostPoint] {
        self.runtimes
            .get(id)
            .map_or(&[][..], |state| state.telemetry_series.as_slice())
    }

    /// The router's latest `router::Metrics` sample for the Traffic tab -
    /// `None` before the first sample arrives (graceful absence, same as the
    /// host meter).
    #[must_use]
    pub fn latest_traffic_sample_for(&self, id: &str) -> Option<&RouterMetricsSample> {
        self.runtimes.get(id)?.latest_traffic_sample.as_ref()
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
