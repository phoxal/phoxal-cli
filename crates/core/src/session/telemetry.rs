//! Terminal-neutral telemetry snapshots consumed by session presentation.
//!
//! Bus adapters populate these bounded records; this module has no bus,
//! process, command, or terminal authority.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

pub const DEFAULT_FRESHNESS_TTL: Duration = Duration::from_secs(3);

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

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct RobotScope {
    pub namespace: String,
    pub robot_id: String,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct DeviceSample {
    pub cpu_pct: Option<f32>,
    pub ram_used_bytes: Option<u64>,
    pub ram_total_bytes: Option<u64>,
    pub swap_used_bytes: Option<u64>,
    pub swap_total_bytes: Option<u64>,
    pub load_1m: Option<f32>,
    pub load_5m: Option<f32>,
    pub load_15m: Option<f32>,
    pub uptime_s: Option<u64>,
    pub disks: Option<Arc<Vec<DeviceDiskSample>>>,
    pub disks_truncated: u32,
    pub window_ns: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeviceDiskSample {
    pub mount_point: String,
    pub file_system: String,
    pub used_bytes: u64,
    pub total_bytes: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TopicMetric {
    pub topic: String,
    pub from_participant: String,
    pub ingress_rate_hz: f32,
    pub count: u64,
    pub aggregate_overflow: bool,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct RouterMetricsSample {
    pub topics: Arc<Vec<TopicMetric>>,
    pub topics_truncated: u32,
    pub throughput_msg_s: f32,
    pub window_ns: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeDirection {
    Publish,
    Subscribe,
    Mixed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeBufferKind {
    Outbound,
    Latest,
    Subscriber,
    Mixed,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RuntimeStepSample {
    pub target_period_ns: u64,
    pub completed: u64,
    pub errors: u64,
    pub mean_duration_ns: u64,
    pub max_duration_ns: u64,
    pub mean_lateness_ns: u64,
    pub max_lateness_ns: u64,
    pub missed_ticks: u64,
    pub overruns: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RuntimeTopicSample {
    pub topic: String,
    pub direction: RuntimeDirection,
    pub buffer_kind: RuntimeBufferKind,
    pub count: u64,
    pub rate_hz: f32,
    pub drops: u64,
    pub latest_overwrites: u64,
    pub bounded_evictions: u64,
    pub capacity: u64,
    pub current_depth: u64,
    pub high_water_depth: u64,
    pub decode_errors: u64,
    pub overflowed_rows: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RuntimePerformanceSample {
    pub sequence: u64,
    pub participant_id: String,
    pub truncated: u32,
    pub window_ns: u64,
    pub step: Option<RuntimeStepSample>,
    pub topics: Arc<Vec<RuntimeTopicSample>>,
    pub overflow: Option<RuntimeTopicSample>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct RuntimePerformanceSummary {
    pub step_rate_hz: Option<f32>,
    pub message_rate_hz: f32,
    pub budget_utilization_pct: Option<f32>,
    pub headroom_ns: Option<i64>,
    pub current_pressure_pct: Option<f32>,
    pub high_water_pressure_pct: Option<f32>,
    pub drops: u64,
    pub decode_errors: u64,
}

impl RuntimePerformanceSample {
    #[must_use]
    pub fn summary(&self) -> RuntimePerformanceSummary {
        let window_s = self.window_ns as f64 / 1_000_000_000.0;
        let step_rate_hz = self
            .step
            .as_ref()
            .and_then(|step| (window_s > 0.0).then_some((step.completed as f64 / window_s) as f32));
        let budget_utilization_pct = self.step.as_ref().and_then(|step| {
            (step.target_period_ns > 0).then_some(
                (step.max_duration_ns as f64 / step.target_period_ns as f64 * 100.0) as f32,
            )
        });
        let headroom_ns = self.step.as_ref().and_then(|step| {
            (step.target_period_ns > 0).then_some({
                let difference =
                    i128::from(step.target_period_ns) - i128::from(step.max_duration_ns);
                i64::try_from(difference).unwrap_or_else(|_| {
                    if difference.is_negative() {
                        i64::MIN
                    } else {
                        i64::MAX
                    }
                })
            })
        });
        let rows = self.topics.iter().chain(self.overflow.iter());
        rows.fold(
            RuntimePerformanceSummary {
                step_rate_hz,
                budget_utilization_pct,
                headroom_ns,
                ..RuntimePerformanceSummary::default()
            },
            |mut summary, row| {
                summary.message_rate_hz += row.rate_hz;
                summary.drops = summary
                    .drops
                    .saturating_add(row.drops)
                    .saturating_add(row.latest_overwrites)
                    .saturating_add(row.bounded_evictions);
                summary.decode_errors = summary.decode_errors.saturating_add(row.decode_errors);
                if row.capacity > 0 {
                    let current =
                        (row.current_depth as f64 / row.capacity as f64 * 100.0).clamp(0.0, 100.0);
                    let high = (row.high_water_depth as f64 / row.capacity as f64 * 100.0)
                        .clamp(0.0, 100.0);
                    summary.current_pressure_pct = Some(
                        summary
                            .current_pressure_pct
                            .map_or(current as f32, |old| old.max(current as f32)),
                    );
                    summary.high_water_pressure_pct = Some(
                        summary
                            .high_water_pressure_pct
                            .map_or(high as f32, |old| old.max(high as f32)),
                    );
                }
                summary
            },
        )
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RuntimeFeedStatus {
    pub capacity_evictions: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct JoypadDevice {
    pub id: String,
    pub name: String,
    pub status: JoypadDeviceStatus,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum JoypadDeviceStatus {
    Ready,
    Disconnected,
    Unsupported,
    #[default]
    Unknown,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct JoypadDevicesSample {
    pub available: Arc<Vec<JoypadDevice>>,
    pub devices_truncated: usize,
    pub selected: Option<String>,
    pub enabled: bool,
    pub unavailable_reason: Option<String>,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ClockSample {
    pub now_ns: u64,
    pub step: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ClockObservation {
    pub latest: Option<ClockSample>,
    pub received_at: Option<Instant>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct MotionSample {
    pub linear_x_mps: f32,
    pub angular_z_radps: f32,
}

#[derive(Debug, Clone, Default)]
pub struct TelemetrySnapshot {
    pub scope: Option<RobotScope>,
    pub clock: Option<Timestamped<ClockSample>>,
    pub device: Option<Timestamped<DeviceSample>>,
    pub router: Option<Timestamped<RouterMetricsSample>>,
    pub router_throughput_history: Vec<Timestamped<f32>>,
    pub runtimes: BTreeMap<String, Timestamped<RuntimePerformanceSample>>,
    pub runtime_status: RuntimeFeedStatus,
    pub joypad: Option<Timestamped<JoypadDevicesSample>>,
    pub motion: Option<Timestamped<MotionSample>>,
}

impl TelemetrySnapshot {
    #[must_use]
    pub fn runtime(
        &self,
        scope: &RobotScope,
        participant_id: &str,
    ) -> Option<&Timestamped<RuntimePerformanceSample>> {
        (self.scope.as_ref() == Some(scope))
            .then(|| self.runtimes.get(participant_id))
            .flatten()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JoypadCommand {
    Select(String),
    SetEnabled(bool),
    Rescan,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn topic(
        buffer_kind: RuntimeBufferKind,
        capacity: u64,
        current: u64,
        high: u64,
    ) -> RuntimeTopicSample {
        RuntimeTopicSample {
            topic: "v1/test".to_string(),
            direction: RuntimeDirection::Publish,
            buffer_kind,
            count: 20,
            rate_hz: 10.0,
            drops: 1,
            latest_overwrites: 2,
            bounded_evictions: 3,
            capacity,
            current_depth: current,
            high_water_depth: high,
            decode_errors: 4,
            overflowed_rows: 0,
        }
    }

    #[test]
    fn runtime_summary_derives_portable_rate_budget_headroom_and_pressure() {
        let sample = RuntimePerformanceSample {
            sequence: 1,
            participant_id: "drive".to_string(),
            truncated: 0,
            window_ns: 2_000_000_000,
            step: Some(RuntimeStepSample {
                target_period_ns: 20_000_000,
                completed: 100,
                max_duration_ns: 15_000_000,
                ..RuntimeStepSample::default()
            }),
            topics: Arc::new(vec![
                topic(RuntimeBufferKind::Outbound, 10, 4, 8),
                // The repeated outbound capacity is deliberately reduced by
                // max pressure, never summed as independent capacity.
                topic(RuntimeBufferKind::Outbound, 10, 2, 5),
            ]),
            overflow: Some(topic(RuntimeBufferKind::Mixed, 0, 99, 99)),
        };

        let summary = sample.summary();
        assert_eq!(summary.step_rate_hz, Some(50.0));
        assert_eq!(summary.message_rate_hz, 30.0);
        assert_eq!(summary.budget_utilization_pct, Some(75.0));
        assert_eq!(summary.headroom_ns, Some(5_000_000));
        assert_eq!(summary.current_pressure_pct, Some(40.0));
        assert_eq!(summary.high_water_pressure_pct, Some(80.0));
        assert_eq!(summary.drops, 18);
        assert_eq!(summary.decode_errors, 12);
    }

    #[test]
    fn event_driven_runtime_keeps_message_rate_without_inventing_a_budget() {
        let sample = RuntimePerformanceSample {
            sequence: 1,
            participant_id: "camera".to_string(),
            truncated: 0,
            window_ns: 1_000_000_000,
            step: None,
            topics: Arc::new(vec![topic(RuntimeBufferKind::Subscriber, 0, 0, 0)]),
            overflow: None,
        };
        let summary = sample.summary();
        assert_eq!(summary.step_rate_hz, None);
        assert_eq!(summary.budget_utilization_pct, None);
        assert_eq!(summary.headroom_ns, None);
        assert_eq!(summary.message_rate_hz, 10.0);
        assert_eq!(summary.current_pressure_pct, None);
    }

    #[test]
    fn runtime_lookup_requires_the_exact_robot_scope_even_for_the_same_id() {
        let now = Instant::now();
        let r1 = crate::session::telemetry::RobotScope {
            namespace: "acme".to_string(),
            robot_id: "r1".to_string(),
        };
        let r2 = crate::session::telemetry::RobotScope {
            namespace: "acme".to_string(),
            robot_id: "r2".to_string(),
        };
        let sample = RuntimePerformanceSample {
            sequence: 1,
            participant_id: "drive".to_string(),
            truncated: 0,
            window_ns: 1,
            step: None,
            topics: Arc::default(),
            overflow: None,
        };
        let snapshot = TelemetrySnapshot {
            scope: Some(r1.clone()),
            runtimes: BTreeMap::from([("drive".to_string(), Timestamped::new(sample, now))]),
            ..TelemetrySnapshot::default()
        };

        assert!(snapshot.runtime(&r1, "drive").is_some());
        assert!(snapshot.runtime(&r2, "drive").is_none());
    }

    #[test]
    fn runtime_pressure_is_clamped_when_hostile_depth_exceeds_capacity() {
        let sample = RuntimePerformanceSample {
            sequence: 1,
            participant_id: "drive".to_string(),
            truncated: 0,
            window_ns: 1,
            step: None,
            topics: Arc::new(vec![topic(
                RuntimeBufferKind::Subscriber,
                2,
                u64::MAX,
                u64::MAX,
            )]),
            overflow: None,
        };
        let summary = sample.summary();
        assert_eq!(summary.current_pressure_pct, Some(100.0));
        assert_eq!(summary.high_water_pressure_pct, Some(100.0));
    }
}
