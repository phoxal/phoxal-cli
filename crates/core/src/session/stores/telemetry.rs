//! Receive-time stamped latest telemetry for the live session model.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use phoxal_api::v1 as state_api;

use crate::session::telemetry::{HostSample, JoypadDevicesSample, RouterMetricsSample};

pub const DEFAULT_FRESHNESS_TTL: Duration = Duration::from_secs(3);
pub const ROUTER_HISTORY_CAPACITY: usize = 60;

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
    router: Option<Timestamped<RouterMetricsSample>>,
    router_throughput_history: VecDeque<Timestamped<f32>>,
    joypad: Option<Timestamped<JoypadDevicesSample>>,
    motion: Option<Timestamped<state_api::motion::State>>,
}

impl TelemetryStore {
    pub fn record_host(&mut self, now: Instant, sample: HostSample) {
        self.host = Some(Timestamped::new(sample, now));
    }

    pub fn record_router(&mut self, now: Instant, sample: RouterMetricsSample) {
        self.router_throughput_history
            .push_back(Timestamped::new(sample.throughput_msg_s, now));
        if self.router_throughput_history.len() > ROUTER_HISTORY_CAPACITY {
            self.router_throughput_history.pop_front();
        }
        self.router = Some(Timestamped::new(sample, now));
    }

    pub fn install_router_history(
        &mut self,
        now: Instant,
        samples: Vec<RouterMetricsSample>,
        current: Option<RouterMetricsSample>,
    ) {
        self.router_throughput_history.clear();
        let keep_from = samples.len().saturating_sub(ROUTER_HISTORY_CAPACITY);
        for sample in &samples[keep_from..] {
            self.router_throughput_history
                .push_back(Timestamped::new(sample.throughput_msg_s, now));
        }
        self.router = current
            .or_else(|| samples.last().cloned())
            .map(|sample| Timestamped::new(sample, now));
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
    pub fn router(&self) -> Option<&Timestamped<RouterMetricsSample>> {
        self.router.as_ref()
    }

    #[must_use]
    pub fn router_throughput_history(&self) -> &VecDeque<Timestamped<f32>> {
        &self.router_throughput_history
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
        let sample = Timestamped::new(RouterMetricsSample::default(), received);
        assert!(!sample.is_stale(received + Duration::from_secs(2), DEFAULT_FRESHNESS_TTL));
        assert!(sample.is_stale(received + Duration::from_secs(4), DEFAULT_FRESHNESS_TTL));
    }

    #[test]
    fn router_history_is_bounded_to_one_minute_of_samples() {
        let mut store = TelemetryStore::default();
        let now = Instant::now();
        for index in 0..(ROUTER_HISTORY_CAPACITY + 3) {
            store.record_router(
                now + Duration::from_secs(index as u64),
                RouterMetricsSample {
                    throughput_msg_s: index as f32,
                    ..RouterMetricsSample::default()
                },
            );
        }
        assert_eq!(
            store.router_throughput_history().len(),
            ROUTER_HISTORY_CAPACITY
        );
        assert_eq!(
            store.router_throughput_history().front().unwrap().value,
            3.0
        );
    }

    #[test]
    fn snapshot_replaces_and_bounds_router_history() {
        let mut store = TelemetryStore::default();
        let now = Instant::now();
        store.record_router(
            now,
            RouterMetricsSample {
                throughput_msg_s: -1.0,
                ..RouterMetricsSample::default()
            },
        );
        let samples = (0..(ROUTER_HISTORY_CAPACITY + 3))
            .map(|index| RouterMetricsSample {
                throughput_msg_s: index as f32,
                ..RouterMetricsSample::default()
            })
            .collect();
        let current = RouterMetricsSample {
            throughput_msg_s: 99.0,
            ..RouterMetricsSample::default()
        };

        store.install_router_history(now, samples, Some(current));

        assert_eq!(
            store.router_throughput_history().len(),
            ROUTER_HISTORY_CAPACITY
        );
        assert_eq!(
            store.router_throughput_history().front().unwrap().value,
            3.0
        );
        assert_eq!(store.router().unwrap().value.throughput_msg_s, 99.0);
    }
}
