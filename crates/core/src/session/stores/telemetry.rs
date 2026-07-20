//! Receive-time stamped latest telemetry for the live session model.

use std::collections::{BTreeMap, VecDeque};
use std::time::{Duration, Instant};

use phoxal_api::v1 as state_api;

use crate::session::telemetry::{
    HostSample, JoypadDevicesSample, RouterMetricsSample, RuntimePerformanceSample,
    ScopedRuntimePerformance,
};

pub const DEFAULT_FRESHNESS_TTL: Duration = Duration::from_secs(3);
pub const ROUTER_HISTORY_CAPACITY: usize = 60;
pub const RUNTIME_HISTORY_CAPACITY: usize = 4096;

#[derive(Debug, Clone, Copy)]
pub struct Timestamped<T> {
    pub value: T,
    pub received_at: Instant,
}

impl<T> Timestamped<T> {
    pub fn new(value: T, received_at: Instant) -> Self {
        Self { value, received_at }
    }

    #[must_use]
    pub fn is_stale(&self, now: Instant, ttl: Duration) -> bool {
        now.saturating_duration_since(self.received_at) > ttl
    }
}

/// Identity of one robot-scoped retained feed. Multiple robots may share the
/// same TUI backend during multi-robot sessions, so snapshot replacement must
/// never clear another robot's retained state.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct RobotScope {
    pub namespace: String,
    pub robot_id: String,
}

#[derive(Debug, Clone, Default)]
struct RouterTelemetry {
    latest: Option<Timestamped<RouterMetricsSample>>,
    throughput_history: VecDeque<Timestamped<f32>>,
}

#[derive(Debug, Clone, Default)]
struct RuntimeTelemetry {
    by_participant: BTreeMap<String, VecDeque<Timestamped<RuntimePerformanceSample>>>,
}

#[derive(Debug, Clone, Default)]
pub struct TelemetryStore {
    host: Option<Timestamped<HostSample>>,
    routers: BTreeMap<RobotScope, RouterTelemetry>,
    runtimes: BTreeMap<RobotScope, RuntimeTelemetry>,
    joypad: Option<Timestamped<JoypadDevicesSample>>,
    motion: Option<Timestamped<state_api::motion::State>>,
}

impl TelemetryStore {
    pub fn record_host(&mut self, now: Instant, sample: HostSample) {
        self.host = Some(Timestamped::new(sample, now));
    }

    pub fn record_router(&mut self, scope: RobotScope, now: Instant, sample: RouterMetricsSample) {
        let router = self.routers.entry(scope).or_default();
        router
            .throughput_history
            .push_back(Timestamped::new(sample.throughput_msg_s, now));
        if router.throughput_history.len() > ROUTER_HISTORY_CAPACITY {
            router.throughput_history.pop_front();
        }
        router.latest = Some(Timestamped::new(sample, now));
    }

    pub fn install_router_history(
        &mut self,
        scope: RobotScope,
        samples: Vec<Timestamped<RouterMetricsSample>>,
        current: Option<Timestamped<RouterMetricsSample>>,
    ) {
        let router = self.routers.entry(scope).or_default();
        router.throughput_history.clear();
        let keep_from = samples.len().saturating_sub(ROUTER_HISTORY_CAPACITY);
        for sample in &samples[keep_from..] {
            router.throughput_history.push_back(Timestamped::new(
                sample.value.throughput_msg_s,
                sample.received_at,
            ));
        }
        router.latest = current
            .or_else(|| samples.last().cloned())
            .map(|sample| Timestamped::new(sample.value, sample.received_at));
    }

    pub fn install_runtime_history(
        &mut self,
        scope: RobotScope,
        now: Instant,
        samples: Vec<RuntimePerformanceSample>,
    ) {
        let runtime = self.runtimes.entry(scope).or_default();
        runtime.by_participant.clear();
        for sample in samples
            .into_iter()
            .rev()
            .take(RUNTIME_HISTORY_CAPACITY)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
        {
            runtime
                .by_participant
                .entry(sample.participant_id.clone())
                .or_default()
                .push_back(Timestamped::new(sample, now));
        }
    }

    pub fn record_runtime(
        &mut self,
        scope: RobotScope,
        now: Instant,
        sample: RuntimePerformanceSample,
    ) {
        let runtime = self.runtimes.entry(scope).or_default();
        runtime
            .by_participant
            .entry(sample.participant_id.clone())
            .or_default()
            .push_back(Timestamped::new(sample, now));
        while runtime
            .by_participant
            .values()
            .map(VecDeque::len)
            .sum::<usize>()
            > RUNTIME_HISTORY_CAPACITY
        {
            let oldest = runtime
                .by_participant
                .iter()
                .filter_map(|(id, history)| {
                    history
                        .front()
                        .map(|sample| (id.clone(), sample.value.sequence))
                })
                .min_by_key(|(_, sequence)| *sequence)
                .map(|(id, _)| id);
            let Some(oldest) = oldest else { break };
            if let Some(history) = runtime.by_participant.get_mut(&oldest) {
                history.pop_front();
                if history.is_empty() {
                    runtime.by_participant.remove(&oldest);
                }
            }
        }
    }

    #[must_use]
    pub fn runtimes(&self, scope: &RobotScope) -> Vec<ScopedRuntimePerformance> {
        self.runtimes
            .get(scope)
            .into_iter()
            .flat_map(|runtime| {
                runtime.by_participant.values().filter_map(|history| {
                    history
                        .back()
                        .cloned()
                        .map(|sample| ScopedRuntimePerformance {
                            namespace: scope.namespace.clone(),
                            robot_id: scope.robot_id.clone(),
                            sample,
                        })
                })
            })
            .collect()
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
    pub fn router(&self, scope: &RobotScope) -> Option<&Timestamped<RouterMetricsSample>> {
        self.routers
            .get(scope)
            .and_then(|router| router.latest.as_ref())
    }

    #[must_use]
    pub fn router_throughput_history(
        &self,
        scope: &RobotScope,
    ) -> impl DoubleEndedIterator<Item = Timestamped<f32>> + '_ {
        self.routers
            .get(scope)
            .into_iter()
            .flat_map(|router| router.throughput_history.iter().copied())
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

    fn scope(robot_id: &str) -> RobotScope {
        RobotScope {
            namespace: "acme".to_string(),
            robot_id: robot_id.to_string(),
        }
    }

    fn runtime(sequence: u64, participant_id: &str) -> RuntimePerformanceSample {
        RuntimePerformanceSample {
            sequence,
            participant_id: participant_id.to_string(),
            truncated: 0,
            window_ns: 1,
            step: None,
            topics: std::sync::Arc::default(),
            overflow: None,
        }
    }

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
                scope("r1"),
                now + Duration::from_secs(index as u64),
                RouterMetricsSample {
                    throughput_msg_s: index as f32,
                    ..RouterMetricsSample::default()
                },
            );
        }
        assert_eq!(
            store.router_throughput_history(&scope("r1")).count(),
            ROUTER_HISTORY_CAPACITY
        );
        assert_eq!(
            store
                .router_throughput_history(&scope("r1"))
                .next()
                .unwrap()
                .value,
            3.0
        );
    }

    #[test]
    fn snapshot_replaces_and_bounds_router_history() {
        let mut store = TelemetryStore::default();
        let now = Instant::now();
        store.record_router(
            scope("r1"),
            now,
            RouterMetricsSample {
                throughput_msg_s: -1.0,
                ..RouterMetricsSample::default()
            },
        );
        let samples = (0..(ROUTER_HISTORY_CAPACITY + 3))
            .map(|index| {
                Timestamped::new(
                    RouterMetricsSample {
                        throughput_msg_s: index as f32,
                        ..RouterMetricsSample::default()
                    },
                    now + Duration::from_secs(index as u64),
                )
            })
            .collect();
        let current = Timestamped::new(
            RouterMetricsSample {
                throughput_msg_s: 99.0,
                ..RouterMetricsSample::default()
            },
            now + Duration::from_secs(99),
        );

        store.install_router_history(scope("r1"), samples, Some(current));

        assert_eq!(
            store.router_throughput_history(&scope("r1")).count(),
            ROUTER_HISTORY_CAPACITY
        );
        assert_eq!(
            store
                .router_throughput_history(&scope("r1"))
                .next()
                .unwrap()
                .value,
            3.0
        );
        assert_eq!(
            store.router(&scope("r1")).unwrap().value.throughput_msg_s,
            99.0
        );
    }

    #[test]
    fn snapshot_replacement_is_scoped_per_robot() {
        let mut store = TelemetryStore::default();
        let now = Instant::now();
        store.record_router(
            scope("r1"),
            now,
            RouterMetricsSample {
                throughput_msg_s: 1.0,
                ..RouterMetricsSample::default()
            },
        );
        store.record_router(
            scope("r2"),
            now + Duration::from_secs(1),
            RouterMetricsSample {
                throughput_msg_s: 2.0,
                ..RouterMetricsSample::default()
            },
        );
        store.install_router_history(
            scope("r1"),
            vec![Timestamped::new(
                RouterMetricsSample {
                    throughput_msg_s: 3.0,
                    ..RouterMetricsSample::default()
                },
                now + Duration::from_secs(2),
            )],
            None,
        );

        assert_eq!(
            store.router(&scope("r1")).unwrap().value.throughput_msg_s,
            3.0
        );
        assert_eq!(
            store.router(&scope("r2")).unwrap().value.throughput_msg_s,
            2.0
        );
        assert_eq!(
            store
                .router_throughput_history(&scope("r1"))
                .map(|sample| sample.value)
                .collect::<Vec<_>>(),
            vec![3.0]
        );
        assert_eq!(
            store
                .router_throughput_history(&scope("r2"))
                .map(|sample| sample.value)
                .collect::<Vec<_>>(),
            vec![2.0]
        );
    }

    #[test]
    fn runtime_snapshot_replacement_is_scoped_and_latest_is_per_participant() {
        let mut store = TelemetryStore::default();
        let now = Instant::now();
        store.install_runtime_history(
            scope("r1"),
            now,
            vec![runtime(1, "drive"), runtime(2, "drive")],
        );
        store.install_runtime_history(scope("r2"), now, vec![runtime(3, "camera")]);
        store.install_runtime_history(scope("r1"), now, vec![runtime(4, "drive")]);

        let runtimes = store.runtimes();
        assert_eq!(runtimes.len(), 2);
        assert!(runtimes.iter().any(|runtime| {
            runtime.robot_id == "r1"
                && runtime.sample.value.participant_id == "drive"
                && runtime.sample.value.sequence == 4
        }));
        assert!(runtimes.iter().any(|runtime| {
            runtime.robot_id == "r2"
                && runtime.sample.value.participant_id == "camera"
                && runtime.sample.value.sequence == 3
        }));
    }
}
