//! Timestamped latest telemetry samples + freshness + bounded histories.
//!
//! This fixes two bugs in today's `tui::logs::LogRouter::record_telemetry`:
//!
//! 1. **Dedup by identity/time, not by equal value.** The old code compared
//!    the new `HostPoint` against the last pushed one with `PartialEq` and
//!    skipped the push if they matched, so a genuinely flat CPU/RAM reading
//!    silently stopped advancing the Resources tab's history - a live,
//!    still-arriving series rendered as if it had frozen. Every call to
//!    [`TelemetryStore::record_host`] appends a new [`HostPoint`] to the
//!    history unconditionally: the identity of "a sample was received" is
//!    the received timestamp, never the numeric payload.
//! 2. **Freshness from receive timestamp, not implicit trust in the cached
//!    latest value.** Every stored latest sample is wrapped in a
//!    [`Timestamped`] carrying the [`Instant`] it was received, so a caller
//!    can ask [`Timestamped::is_stale`]/[`Timestamped::freshness`] instead of
//!    rendering a long-cached sample as if it were still live.
//!
//! Time is always passed in explicitly (`now: Instant` on every `record_*`
//! and freshness check) rather than captured internally via `Instant::now()`,
//! so tests stay deterministic without real sleeps.
//!
//! This store is a plain data structure: independent of ratatui and of
//! `telemetry::TelemetryBackend` (today's live-feed-holding type). Wiring the
//! live bus feeds into this store instead of `TelemetryBackend` is a later
//! slice's job.

use std::collections::{BTreeMap, VecDeque};
use std::time::{Duration, Instant};

use crate::telemetry::{HostSample, JoypadDevicesSample, ProcessSample, RouterMetricsSample};

/// Bound on the Resources tab's rolling host-sample history. Matches the
/// existing `tui::logs::TELEMETRY_SERIES_CAPACITY` value: at the ~1s
/// `tool-telemetry` sample cadence, 120 points covers a 2-minute window -
/// enough for a sparkline to read as a trend without growing unboundedly
/// over a long session.
pub const HOST_HISTORY_CAPACITY: usize = 120;

/// Default freshness window for a stored latest sample. Several multiples
/// of the ~1s `tool-telemetry`/`tool-router` publish cadence (see
/// [`HOST_HISTORY_CAPACITY`]'s docs), so ordinary scheduling jitter between
/// publish and redraw never flags a genuinely-live sample stale - mirrors
/// the reasoning behind `supervisor::HEARTBEAT_STALE_TIMEOUT`, just tighter
/// since telemetry is expected at roughly 1 Hz rather than heartbeat's
/// coarser cadence.
pub const DEFAULT_FRESHNESS_TTL: Duration = Duration::from_secs(3);

/// One `telemetry::Host` sample recorded into the Resources tab's rolling
/// history. Deliberately without `PartialEq` - nothing in this module may
/// compare two points for equality, since doing so is exactly the bug this
/// store fixes (see the module docs).
#[derive(Debug, Clone, Copy)]
pub struct HostPoint {
    pub received_at: Instant,
    pub cpu_pct: f32,
    pub ram_used_bytes: u64,
    pub ram_total_bytes: u64,
}

/// Whether a [`Timestamped`] sample is still within its freshness window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Freshness {
    Fresh,
    Stale,
}

/// A latest-value sample plus the monotonic instant it was received.
#[derive(Debug, Clone, Copy)]
pub struct Timestamped<T> {
    pub value: T,
    pub received_at: Instant,
}

impl<T> Timestamped<T> {
    fn new(value: T, received_at: Instant) -> Self {
        Self { value, received_at }
    }

    /// Whether this sample is older than `ttl` as of `now`. Uses
    /// `Instant::duration_since`, which saturates to zero rather than
    /// panicking if `received_at` is (spuriously, per platform monotonic
    /// clock imprecision) later than `now`, so this is always safe to call.
    #[must_use]
    pub fn is_stale(&self, now: Instant, ttl: Duration) -> bool {
        now.duration_since(self.received_at) > ttl
    }

    /// [`Freshness::Stale`] iff [`Self::is_stale`].
    #[must_use]
    pub fn freshness(&self, now: Instant, ttl: Duration) -> Freshness {
        if self.is_stale(now, ttl) {
            Freshness::Stale
        } else {
            Freshness::Fresh
        }
    }
}

/// Timestamped latest telemetry samples plus bounded histories, deduped by
/// sample identity/time rather than by equal value (see module docs).
#[derive(Debug, Clone, Default)]
pub struct TelemetryStore {
    host: Option<Timestamped<HostSample>>,
    host_history: VecDeque<HostPoint>,
    process_by_participant: BTreeMap<String, Timestamped<ProcessSample>>,
    router: Option<Timestamped<RouterMetricsSample>>,
    joypad: Option<Timestamped<JoypadDevicesSample>>,
}

impl TelemetryStore {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a `telemetry::Host` sample received at `now`: overwrites the
    /// latest value AND unconditionally appends a [`HostPoint`] to the
    /// rolling history - never gated on whether the payload differs from
    /// the previous sample. A flat/constant measurement (a genuinely idle
    /// host) MUST keep advancing the series exactly like a changing one; the
    /// old value-equality dedup that suppressed this is the bug this method
    /// replaces.
    pub fn record_host(&mut self, now: Instant, sample: HostSample) {
        self.host = Some(Timestamped::new(sample, now));
        push_bounded_history(
            &mut self.host_history,
            HostPoint {
                received_at: now,
                cpu_pct: sample.cpu_pct,
                ram_used_bytes: sample.ram_used_bytes,
                ram_total_bytes: sample.ram_total_bytes,
            },
        );
    }

    /// Record a `telemetry::Process` sample for `participant`, received at
    /// `now`. Only the latest value per participant is kept - no history -
    /// per this slice's YAGNI guidance (nothing in the plan plots a
    /// per-process history today).
    pub fn record_process(&mut self, now: Instant, participant: String, sample: ProcessSample) {
        self.process_by_participant
            .insert(participant, Timestamped::new(sample, now));
    }

    /// Record the router's latest `router::Metrics` sample, received at
    /// `now`. Latest-only: the Traffic tab already renders the full topic
    /// breakdown from one sample, so no history is kept (YAGNI, matching
    /// `tui::logs`'s original `latest_traffic_sample` design).
    pub fn record_router(&mut self, now: Instant, sample: RouterMetricsSample) {
        self.router = Some(Timestamped::new(sample, now));
    }

    /// Record the joypad tool's latest device-state sample, received at
    /// `now`. Latest-only - the Devices tab always shows the current
    /// authoritative selection, never a trend.
    pub fn record_joypad(&mut self, now: Instant, sample: JoypadDevicesSample) {
        self.joypad = Some(Timestamped::new(sample, now));
    }

    /// The latest `Host` sample plus its receive time, or `None` before the
    /// first sample arrives.
    #[must_use]
    pub fn host(&self) -> Option<&Timestamped<HostSample>> {
        self.host.as_ref()
    }

    /// The rolling host-sample history, oldest first, as a contiguous slice.
    /// `&mut self` because `VecDeque::make_contiguous` needs it - the
    /// redraw path already holds the store mutably.
    #[must_use]
    pub fn host_history(&mut self) -> &[HostPoint] {
        self.host_history.make_contiguous()
    }

    /// The latest `Process` sample for `participant` plus its receive time,
    /// or `None` if this participant has never published one.
    #[must_use]
    pub fn process(&self, participant: &str) -> Option<&Timestamped<ProcessSample>> {
        self.process_by_participant.get(participant)
    }

    /// Every participant's latest `Process` sample plus its receive time -
    /// the full map, for a caller (`telemetry::TelemetryBackend::snapshot`)
    /// that needs to hand the whole set to a renderer rather than look up
    /// one participant at a time.
    #[must_use]
    pub fn process_all(&self) -> &BTreeMap<String, Timestamped<ProcessSample>> {
        &self.process_by_participant
    }

    /// The router's latest `Metrics` sample plus its receive time, or `None`
    /// before the first sample arrives.
    #[must_use]
    pub fn router(&self) -> Option<&Timestamped<RouterMetricsSample>> {
        self.router.as_ref()
    }

    /// The joypad tool's latest device-state sample plus its receive time,
    /// or `None` before the first sample arrives.
    #[must_use]
    pub fn joypad(&self) -> Option<&Timestamped<JoypadDevicesSample>> {
        self.joypad.as_ref()
    }
}

fn push_bounded_history(history: &mut VecDeque<HostPoint>, point: HostPoint) {
    history.push_back(point);
    if history.len() > HOST_HISTORY_CAPACITY {
        history.pop_front();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host_sample() -> HostSample {
        HostSample {
            cpu_pct: 12.5,
            ram_used_bytes: 100,
            ram_total_bytes: 200,
            load_1m: 0.3,
            window_ns: 1_000_000_000,
        }
    }

    #[test]
    fn history_advances_on_identical_consecutive_values() {
        let mut store = TelemetryStore::new();
        let t0 = Instant::now();
        let sample = host_sample();

        // Same payload, different receive instants: the old value-equality
        // dedup would have suppressed the second push. It must not.
        store.record_host(t0, sample);
        store.record_host(t0 + Duration::from_millis(500), sample);

        assert_eq!(
            store.host_history().len(),
            2,
            "a flat/unchanging sample must still advance the series"
        );
    }

    #[test]
    fn history_keeps_advancing_across_many_identical_samples() {
        let mut store = TelemetryStore::new();
        let t0 = Instant::now();
        let sample = host_sample();
        for i in 0..10u32 {
            store.record_host(t0 + Duration::from_millis(u64::from(i) * 500), sample);
        }
        assert_eq!(store.host_history().len(), 10);
    }

    #[test]
    fn latest_host_value_is_the_most_recently_recorded_one() {
        let mut store = TelemetryStore::new();
        let t0 = Instant::now();
        store.record_host(t0, host_sample());
        let mut second = host_sample();
        second.cpu_pct = 99.0;
        store.record_host(t0 + Duration::from_secs(1), second);
        assert_eq!(store.host().unwrap().value.cpu_pct, 99.0);
    }

    #[test]
    fn freshness_is_fresh_within_ttl_and_stale_beyond_it() {
        let mut store = TelemetryStore::new();
        let t0 = Instant::now();
        let ttl = Duration::from_secs(2);
        store.record_host(t0, host_sample());

        let sample = store.host().expect("host sample recorded");
        assert_eq!(
            sample.freshness(t0 + ttl / 2, ttl),
            Freshness::Fresh,
            "well within the TTL window"
        );
        assert!(!sample.is_stale(t0 + ttl / 2, ttl));

        assert_eq!(
            sample.freshness(t0 + ttl * 2, ttl),
            Freshness::Stale,
            "well past the TTL window"
        );
        assert!(sample.is_stale(t0 + ttl * 2, ttl));
    }

    #[test]
    fn host_history_is_bounded_to_capacity() {
        let mut store = TelemetryStore::new();
        let t0 = Instant::now();
        let sample = host_sample();
        for i in 0..(HOST_HISTORY_CAPACITY + 25) {
            store.record_host(t0 + Duration::from_millis(i as u64 * 500), sample);
        }
        assert_eq!(store.host_history().len(), HOST_HISTORY_CAPACITY);
        // Oldest points evicted, newest retained: the last recorded instant
        // must still be present as the final entry.
        let last_recorded = t0 + Duration::from_millis((HOST_HISTORY_CAPACITY + 24) as u64 * 500);
        assert_eq!(
            store.host_history().last().unwrap().received_at,
            last_recorded
        );
    }

    #[test]
    fn process_samples_are_demuxed_by_participant() {
        let mut store = TelemetryStore::new();
        let t0 = Instant::now();
        store.record_process(
            t0,
            "drive".to_string(),
            ProcessSample {
                cpu_pct: 1.0,
                rss_bytes: 10,
                window_ns: 1,
            },
        );
        store.record_process(
            t0,
            "mission".to_string(),
            ProcessSample {
                cpu_pct: 2.0,
                rss_bytes: 20,
                window_ns: 1,
            },
        );
        assert_eq!(store.process("drive").unwrap().value.cpu_pct, 1.0);
        assert_eq!(store.process("mission").unwrap().value.cpu_pct, 2.0);
        assert!(store.process("unknown").is_none());
    }

    #[test]
    fn router_and_joypad_are_latest_only_and_graceful_before_first_sample() {
        let mut store = TelemetryStore::new();
        assert!(store.router().is_none());
        assert!(store.joypad().is_none());

        let t0 = Instant::now();
        store.record_router(t0, RouterMetricsSample::default());
        store.record_joypad(t0, JoypadDevicesSample::default());
        assert!(store.router().is_some());
        assert!(store.joypad().is_some());
    }

    #[test]
    fn empty_store_reports_no_samples_and_empty_history() {
        let mut store = TelemetryStore::new();
        assert!(store.host().is_none());
        assert!(store.host_history().is_empty());
        assert!(store.process("anything").is_none());
    }
}
