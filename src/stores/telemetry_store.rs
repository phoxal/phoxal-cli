//! Receive-time stamped latest telemetry for the live session model.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use phoxal_api::v1 as state_api;

use crate::telemetry::{HostSample, JoypadDevicesSample, ProcessSample, RouterMetricsSample};

pub const DEFAULT_FRESHNESS_TTL: Duration = Duration::from_secs(3);

#[derive(Debug, Clone, Copy)]
pub struct Timestamped<T> {
    pub value: T,
    pub received_at: Instant,
}

impl<T> Timestamped<T> {
    fn new(value: T, received_at: Instant) -> Self {
        Self { value, received_at }
    }

    #[must_use]
    pub fn is_stale(&self, now: Instant, ttl: Duration) -> bool {
        now.saturating_duration_since(self.received_at) > ttl
    }
}

#[derive(Debug, Clone, Default)]
pub struct TelemetryStore {
    host: Option<Timestamped<HostSample>>,
    process_by_participant: BTreeMap<String, Timestamped<ProcessSample>>,
    router: Option<Timestamped<RouterMetricsSample>>,
    joypad: Option<Timestamped<JoypadDevicesSample>>,
    motion: Option<Timestamped<state_api::motion::State>>,
}

impl TelemetryStore {
    pub fn record_host(&mut self, now: Instant, sample: HostSample) {
        self.host = Some(Timestamped::new(sample, now));
    }

    pub fn record_process(&mut self, now: Instant, participant: String, sample: ProcessSample) {
        self.process_by_participant
            .insert(participant, Timestamped::new(sample, now));
    }

    pub fn record_router(&mut self, now: Instant, sample: RouterMetricsSample) {
        self.router = Some(Timestamped::new(sample, now));
    }

    pub fn record_joypad(&mut self, now: Instant, sample: JoypadDevicesSample) {
        self.joypad = Some(Timestamped::new(sample, now));
    }

    pub fn record_motion(&mut self, now: Instant, sample: state_api::motion::State) {
        self.motion = Some(Timestamped::new(sample, now));
    }

    #[must_use]
    pub fn host(&self) -> Option<&Timestamped<HostSample>> {
        self.host.as_ref()
    }

    #[must_use]
    pub fn process_all(&self) -> &BTreeMap<String, Timestamped<ProcessSample>> {
        &self.process_by_participant
    }

    #[must_use]
    pub fn router(&self) -> Option<&Timestamped<RouterMetricsSample>> {
        self.router.as_ref()
    }

    #[must_use]
    pub fn joypad(&self) -> Option<&Timestamped<JoypadDevicesSample>> {
        self.joypad.as_ref()
    }

    #[must_use]
    pub fn motion(&self) -> Option<&Timestamped<state_api::motion::State>> {
        self.motion.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn freshness_uses_receive_time() {
        let received = Instant::now();
        let sample = Timestamped::new(ProcessSample::default(), received);
        assert!(!sample.is_stale(received + Duration::from_secs(2), DEFAULT_FRESHNESS_TTL));
        assert!(sample.is_stale(received + Duration::from_secs(4), DEFAULT_FRESHNESS_TTL));
    }

    #[test]
    fn process_samples_are_demultiplexed_by_runtime() {
        let mut store = TelemetryStore::default();
        let now = Instant::now();
        store.record_process(
            now,
            "drive".to_string(),
            ProcessSample {
                cpu_pct: 1.0,
                rss_bytes: 2,
                window_ns: 3,
            },
        );
        assert_eq!(
            store
                .process_all()
                .get("drive")
                .map(|sample| sample.value.rss_bytes),
            Some(2)
        );
    }
}
