//! Disposable graph sources and their stateless ingress adapter.

pub(crate) mod bus;
pub(crate) mod clock;
pub(crate) mod device;
pub(crate) mod input;
pub(crate) mod liveliness;
pub(crate) mod logs;
pub(crate) mod motion;
pub(crate) mod runtimes;

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::convert::Infallible;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use phoxal::bus::{
    CommandPublisher, ContractBody, DEFAULT_QUERY_TIMEOUT, Publish, Querier, Subscribe, Subscriber,
    Topic,
};
use phoxal::raw::Bus;
use phoxal_api::v0_1 as api;
use phoxal_api::v0_1 as state_api;
use tokio::sync::{Notify, mpsc};

use crate::reconcile::{Cursor, ReconcileOutcome, Reconciler, RetryBackoff, Sequenced};
use phoxal_cli_observation::{
    DeviceDiskSample, DeviceSample, JoypadDevice, JoypadDeviceStatus, JoypadDevicesSample,
    RouterMetricsSample, RuntimeBufferKind, RuntimeDirection, RuntimeFeedStatus,
    RuntimePerformanceSample, RuntimeStepSample, RuntimeTopicSample, TopicMetric,
    sanitize_terminal_text,
};
use phoxal_cli_observation::{RobotScope, SourceStatus};

const MAX_DEVICE_DISKS: usize = 32;
const MAX_ROUTER_TOPICS: usize = 256;
const MAX_JOYPAD_DEVICES: usize = 64;

#[derive(Debug, Clone, Copy)]
pub(crate) struct Timestamped<T> {
    pub(crate) value: T,
    pub(crate) received_at: Instant,
}

impl<T> Timestamped<T> {
    pub(crate) fn new(value: T, received_at: Instant) -> Self {
        Self { value, received_at }
    }
}
const MAX_REMOTE_TEXT_CHARS: usize = 256;

fn device_sample_from(record: state_api::tool::device::Record) -> DeviceSample {
    let body = record.sample;
    let received_disks = body.disks.as_ref().map_or(0, Vec::len);
    let disks = body.disks.map(|disks| {
        Arc::new(
            disks
                .into_iter()
                .take(MAX_DEVICE_DISKS)
                .map(|disk| DeviceDiskSample {
                    mount_point: bounded_remote_text(&disk.mount_point),
                    file_system: bounded_remote_text(&disk.file_system),
                    used_bytes: disk.used_bytes,
                    total_bytes: disk.total_bytes,
                })
                .collect::<Vec<_>>(),
        )
    });
    let locally_truncated =
        received_disks.saturating_sub(disks.as_ref().map_or(0, |rows| rows.len()));
    DeviceSample {
        cpu_pct: body.cpu_pct,
        ram_used_bytes: body.ram_used_bytes,
        ram_total_bytes: body.ram_total_bytes,
        swap_used_bytes: body.swap_used_bytes,
        swap_total_bytes: body.swap_total_bytes,
        load_1m: body.load_1m,
        load_5m: body.load_5m,
        load_15m: body.load_15m,
        uptime_s: body.uptime_s,
        disks,
        disks_truncated: record
            .truncated
            .saturating_add(u32::try_from(locally_truncated).unwrap_or(u32::MAX)),
        window_ns: body.window_ns,
    }
}

fn topic_metric_from(body: state_api::tool::bus::TopicMetric) -> TopicMetric {
    let aggregate_overflow = body.topic.is_empty() && body.from_participant.is_empty();
    TopicMetric {
        topic: if aggregate_overflow {
            "Other/unobserved traffic".to_string()
        } else {
            bounded_remote_text(&body.topic)
        },
        from_participant: if aggregate_overflow {
            "multiple".to_string()
        } else {
            bounded_remote_text(&body.from_participant)
        },
        ingress_rate_hz: body.ingress_rate_hz,
        count: body.count,
        aggregate_overflow,
    }
}

fn router_metrics_sample_from(body: state_api::tool::bus::Window) -> RouterMetricsSample {
    let received_topics = body.topics.len();
    let keep = if received_topics > MAX_ROUTER_TOPICS {
        MAX_ROUTER_TOPICS.saturating_sub(1)
    } else {
        MAX_ROUTER_TOPICS
    };
    let mut wire_topics = body.topics;
    let dropped = if wire_topics.len() > keep {
        wire_topics.split_off(keep)
    } else {
        Vec::new()
    };
    let dropped_rate = dropped.iter().map(|metric| metric.ingress_rate_hz).sum();
    let dropped_count = dropped
        .iter()
        .fold(0_u64, |total, metric| total.saturating_add(metric.count));
    let mut topics = wire_topics
        .into_iter()
        .map(topic_metric_from)
        .collect::<Vec<_>>();
    if received_topics > keep {
        if let Some(overflow) = topics.iter_mut().find(|metric| metric.aggregate_overflow) {
            overflow.ingress_rate_hz += dropped_rate;
            overflow.count = overflow.count.saturating_add(dropped_count);
        } else {
            topics.push(TopicMetric {
                topic: "Other/unobserved traffic".to_string(),
                from_participant: "multiple".to_string(),
                ingress_rate_hz: dropped_rate,
                count: dropped_count,
                aggregate_overflow: true,
            });
        }
    }
    RouterMetricsSample {
        topics: Arc::new(topics),
        topics_truncated: u32::try_from(received_topics.saturating_sub(keep)).unwrap_or(u32::MAX),
        throughput_msg_s: body.throughput_msg_s,
        window_ns: body.window_ns,
    }
}

fn joypad_device_from(body: api::joypad::Device) -> JoypadDevice {
    JoypadDevice {
        id: bounded_remote_text(&body.id),
        name: bounded_remote_text(&body.name),
        status: match body.status {
            api::joypad::DeviceStatus::Ready => JoypadDeviceStatus::Ready,
            api::joypad::DeviceStatus::Disconnected => JoypadDeviceStatus::Disconnected,
            api::joypad::DeviceStatus::Unsupported => JoypadDeviceStatus::Unsupported,
        },
    }
}

/// The joypad tool's latest published device state - `selected` is the
/// authoritative selection (the tool's own acknowledgement), never a local
/// client guess.
fn joypad_devices_sample_from(body: api::joypad::Devices) -> JoypadDevicesSample {
    let received_devices = body.available.len();
    let available = body
        .available
        .into_iter()
        .take(MAX_JOYPAD_DEVICES)
        .map(joypad_device_from)
        .collect::<Vec<_>>();
    JoypadDevicesSample {
        devices_truncated: received_devices.saturating_sub(available.len()),
        available: Arc::new(available),
        selected: body.selected.map(|id| bounded_remote_text(&id)),
        enabled: body.enabled,
        unavailable_reason: body
            .unavailable_reason
            .map(|reason| bounded_remote_text(&reason)),
        last_error: body.last_error.map(|error| bounded_remote_text(&error)),
    }
}

fn bounded_remote_text(text: &str) -> String {
    sanitize_terminal_text(text)
        .chars()
        .take(MAX_REMOTE_TEXT_CHARS)
        .collect()
}

fn runtime_topic_from(body: state_api::tool::RuntimeTopic) -> RuntimeTopicSample {
    RuntimeTopicSample {
        topic: if body.topic.is_empty() {
            "Other/unobserved topics".to_string()
        } else {
            bounded_remote_text(&body.topic)
        },
        direction: match body.direction {
            state_api::tool::RuntimeDirection::Publish => RuntimeDirection::Publish,
            state_api::tool::RuntimeDirection::Subscribe => RuntimeDirection::Subscribe,
            state_api::tool::RuntimeDirection::Mixed => RuntimeDirection::Mixed,
        },
        buffer_kind: match body.buffer_kind {
            state_api::tool::RuntimeBufferKind::Outbound => RuntimeBufferKind::Outbound,
            state_api::tool::RuntimeBufferKind::Latest => RuntimeBufferKind::Latest,
            state_api::tool::RuntimeBufferKind::Subscriber => RuntimeBufferKind::Subscriber,
            state_api::tool::RuntimeBufferKind::Mixed => RuntimeBufferKind::Mixed,
        },
        count: body.count,
        rate_hz: body.rate_hz,
        drops: body.drops,
        latest_overwrites: body.latest_overwrites,
        bounded_evictions: body.bounded_evictions,
        capacity: body.capacity,
        current_depth: body.current_depth,
        high_water_depth: body.high_water_depth,
        decode_errors: body.decode_errors,
        overflowed_rows: body.overflowed_rows,
    }
}

fn runtime_record_from(body: state_api::tool::runtime::Record) -> RuntimePerformanceSample {
    RuntimePerformanceSample {
        sequence: body.sequence,
        participant_id: bounded_remote_text(&body.participant_id),
        truncated: body.truncated,
        window_ns: body.window_ns,
        step: body.step.map(|step| RuntimeStepSample {
            target_period_ns: step.target_period_ns,
            completed: step.completed,
            errors: step.errors,
            mean_duration_ns: step.mean_duration_ns,
            max_duration_ns: step.max_duration_ns,
            mean_lateness_ns: step.mean_lateness_ns,
            max_lateness_ns: step.max_lateness_ns,
            missed_ticks: step.missed_ticks,
            overruns: step.overruns,
        }),
        topics: Arc::new(body.topics.into_iter().map(runtime_topic_from).collect()),
        overflow: body.overflow.map(runtime_topic_from),
    }
}

/// Stateless ingress adapter from reconciled source data to the client-owned
/// store updater. Retained values exist only in the stores behind the ports.
#[derive(Debug, Clone)]
pub struct TelemetryBackend {
    updates: Arc<TelemetryMailbox>,
}

#[derive(Debug)]
pub(crate) enum TelemetryUpdate {
    Clock(phoxal_cli_observation::ClockSample),
    Device(RobotScope, DeviceSample),
    Router(RobotScope, Timestamped<RouterMetricsSample>),
    Routers(RobotScope, Vec<Timestamped<RouterMetricsSample>>),
    Runtimes(RobotScope, Vec<RuntimePerformanceSample>, RuntimeFeedStatus),
    Runtime(RobotScope, RuntimePerformanceSample),
    Joypads(JoypadDevicesSample),
    Motion(state_api::motion::State),
    Health(String, SourceStatus),
}

const TELEMETRY_HISTORY_CAPACITY: usize = 512;

#[derive(Debug, Default)]
struct TelemetryPending {
    latest: BTreeMap<String, TelemetryUpdate>,
    history: VecDeque<TelemetryUpdate>,
    dropped: u64,
}

#[derive(Debug, Default)]
pub(crate) struct TelemetryMailbox {
    pending: Mutex<TelemetryPending>,
    wake: Notify,
}

pub(crate) struct TelemetryBatch {
    pub(crate) updates: Vec<TelemetryUpdate>,
    pub(crate) dropped: u64,
}

impl TelemetryMailbox {
    fn push(&self, update: TelemetryUpdate) {
        let mut pending = self.pending.lock().unwrap_or_else(PoisonError::into_inner);
        match telemetry_slot(&update) {
            Some(slot) => {
                pending.latest.insert(slot, update);
            }
            None => {
                if pending.history.len() == TELEMETRY_HISTORY_CAPACITY {
                    pending.history.pop_front();
                    pending.dropped = pending.dropped.saturating_add(1);
                }
                pending.history.push_back(update);
            }
        }
        drop(pending);
        self.wake.notify_one();
    }

    pub(crate) async fn recv(&self) -> TelemetryBatch {
        loop {
            let notified = self.wake.notified();
            {
                let mut pending = self.pending.lock().unwrap_or_else(PoisonError::into_inner);
                if !pending.latest.is_empty() || !pending.history.is_empty() {
                    let mut updates = pending.history.drain(..).collect::<Vec<_>>();
                    updates.extend(std::mem::take(&mut pending.latest).into_values());
                    return TelemetryBatch {
                        updates,
                        dropped: pending.dropped,
                    };
                }
            }
            notified.await;
        }
    }

    #[cfg(test)]
    fn pending_shape(&self) -> (usize, usize, u64) {
        let pending = self.pending.lock().unwrap_or_else(PoisonError::into_inner);
        (pending.latest.len(), pending.history.len(), pending.dropped)
    }
}

fn telemetry_slot(update: &TelemetryUpdate) -> Option<String> {
    match update {
        TelemetryUpdate::Clock(_) => Some("clock".to_string()),
        TelemetryUpdate::Device(scope, _) => {
            Some(format!("device:{}/{}", scope.namespace, scope.robot_id))
        }
        TelemetryUpdate::Joypads(_) => Some("joypads".to_string()),
        TelemetryUpdate::Motion(_) => Some("motion".to_string()),
        TelemetryUpdate::Health(source, _) => Some(format!("health:{source}")),
        TelemetryUpdate::Router(_, _)
        | TelemetryUpdate::Routers(_, _)
        | TelemetryUpdate::Runtimes(_, _, _)
        | TelemetryUpdate::Runtime(_, _) => None,
    }
}

impl TelemetryBackend {
    pub(crate) fn with_updates(updates: Arc<TelemetryMailbox>) -> Self {
        Self { updates }
    }

    pub(crate) fn record_clock(&self, sample: phoxal_cli_observation::ClockSample) {
        self.updates.push(TelemetryUpdate::Clock(sample));
    }

    pub(crate) fn record_device(&self, scope: RobotScope, sample: DeviceSample) {
        self.updates.push(TelemetryUpdate::Device(scope, sample));
    }

    pub(crate) fn record_router_at(
        &self,
        scope: RobotScope,
        received_at: Instant,
        sample: RouterMetricsSample,
    ) {
        self.updates.push(TelemetryUpdate::Router(
            scope,
            Timestamped::new(sample, received_at),
        ));
    }

    pub(crate) fn install_router(
        &self,
        scope: RobotScope,
        mut samples: Vec<Timestamped<RouterMetricsSample>>,
        current: Option<Timestamped<RouterMetricsSample>>,
    ) {
        if let Some(current) = current {
            samples.push(current);
        }
        self.updates.push(TelemetryUpdate::Routers(scope, samples));
    }

    pub(crate) fn install_runtimes(
        &self,
        scope: RobotScope,
        samples: Vec<RuntimePerformanceSample>,
        status: RuntimeFeedStatus,
    ) {
        self.updates
            .push(TelemetryUpdate::Runtimes(scope, samples, status));
    }

    pub(crate) fn record_runtime(&self, scope: RobotScope, sample: RuntimePerformanceSample) {
        self.updates.push(TelemetryUpdate::Runtime(scope, sample));
    }

    pub(crate) fn record_joypad(&self, sample: JoypadDevicesSample) {
        self.updates.push(TelemetryUpdate::Joypads(sample));
    }

    pub(crate) fn record_motion(&self, sample: state_api::motion::State) {
        self.updates.push(TelemetryUpdate::Motion(sample));
    }

    pub(crate) fn record_health(&self, source: impl Into<String>, status: SourceStatus) {
        self.updates
            .push(TelemetryUpdate::Health(source.into(), status));
    }
}

#[cfg(test)]
mod telemetry_mailbox_tests {
    use super::*;

    #[tokio::test]
    async fn blocked_consumer_keeps_latest_state_and_bounded_history() {
        let mailbox = Arc::new(TelemetryMailbox::default());
        let telemetry = TelemetryBackend::with_updates(mailbox.clone());
        for index in 0..100_000_u64 {
            telemetry.record_clock(phoxal_cli_observation::ClockSample {
                now_ns: index,
                step: index,
            });
            telemetry.record_runtime(
                RobotScope {
                    namespace: "lab".into(),
                    robot_id: "rover".into(),
                },
                RuntimePerformanceSample {
                    sequence: index,
                    participant_id: "drive".into(),
                    truncated: 0,
                    window_ns: 1,
                    step: None,
                    topics: Arc::new(Vec::new()),
                    overflow: None,
                },
            );
        }
        let batch = mailbox.recv().await;
        assert_eq!(batch.updates.len(), TELEMETRY_HISTORY_CAPACITY + 1);
        assert_eq!(
            batch.dropped,
            100_000_u64.saturating_sub(TELEMETRY_HISTORY_CAPACITY as u64)
        );
        assert!(batch.updates.iter().any(|update| matches!(
            update,
            TelemetryUpdate::Clock(sample) if sample.now_ns == 99_999
        )));
    }

    #[tokio::test]
    async fn blocked_attachment_event_delivery_cannot_grow_ingress() {
        let mailbox = Arc::new(TelemetryMailbox::default());
        let telemetry = TelemetryBackend::with_updates(mailbox.clone());
        let (event_tx, _event_rx) = mpsc::channel::<()>(1);
        event_tx.send(()).await.unwrap();
        let (blocked_tx, blocked_rx) = tokio::sync::oneshot::channel();
        let consumer_mailbox = mailbox.clone();
        let consumer = tokio::spawn(async move {
            let _ = consumer_mailbox.recv().await;
            let _ = blocked_tx.send(());
            let _ = event_tx.send(()).await;
        });
        telemetry.record_clock(phoxal_cli_observation::ClockSample { now_ns: 1, step: 1 });
        blocked_rx.await.unwrap();

        for index in 0..100_000_u64 {
            telemetry.record_clock(phoxal_cli_observation::ClockSample {
                now_ns: index,
                step: index,
            });
            telemetry.record_runtime(
                RobotScope {
                    namespace: "lab".into(),
                    robot_id: "rover".into(),
                },
                RuntimePerformanceSample {
                    sequence: index,
                    participant_id: "drive".into(),
                    truncated: 0,
                    window_ns: 1,
                    step: None,
                    topics: Arc::new(Vec::new()),
                    overflow: None,
                },
            );
        }

        let (latest, history, dropped) = mailbox.pending_shape();
        assert_eq!(latest, 1);
        assert_eq!(history, TELEMETRY_HISTORY_CAPACITY);
        assert_eq!(
            dropped,
            100_000_u64.saturating_sub(TELEMETRY_HISTORY_CAPACITY as u64)
        );
        consumer.abort();
    }

    #[tokio::test]
    async fn latest_health_is_applied_after_buffered_samples() {
        let mailbox = Arc::new(TelemetryMailbox::default());
        let telemetry = TelemetryBackend::with_updates(mailbox.clone());
        let scope = RobotScope {
            namespace: "lab".into(),
            robot_id: "rover".into(),
        };
        telemetry.record_runtime(
            scope,
            RuntimePerformanceSample {
                sequence: 1,
                participant_id: "drive".into(),
                truncated: 0,
                window_ns: 1,
                step: None,
                topics: Arc::new(Vec::new()),
                overflow: None,
            },
        );
        telemetry.record_health("runtimes:lab/rover", SourceStatus::Failed);
        let batch = mailbox.recv().await;
        assert!(matches!(
            batch.updates.last(),
            Some(TelemetryUpdate::Health(source, SourceStatus::Failed))
                if source == "runtimes:lab/rover"
        ));
    }
}

pub(crate) async fn control_state_feed_loop(bus: Bus, telemetry: &TelemetryBackend) -> Result<()> {
    let motion_topic = Topic::<Subscribe<state_api::motion::State>>::new_static(
        <state_api::motion::State as ContractBody>::TOPIC,
    );
    let motion = Subscriber::new(&bus, &motion_topic, 32).await?;
    loop {
        telemetry.record_motion(motion.recv().await?.body);
    }
}

/// Reconcile one robot root's retained device observation with its live
/// follow feed. Device totals retain both their project/deployment identity
/// and robot-root attribution and are never attributed to a runtime.
pub(crate) async fn device_feed_loop(
    bus: &Bus,
    scope: &RobotScope,
    telemetry: &TelemetryBackend,
) -> Result<Infallible> {
    let follow_topic = state_api::topic::client().tool().device().follow();
    let subscriber =
        Subscriber::<state_api::tool::device::Follow>::new(bus, &follow_topic, 128).await?;
    let snapshot_topic = state_api::topic::client().tool().device().snapshot();
    let querier = Querier::<
        state_api::tool::device::SnapshotRequest,
        state_api::tool::device::Snapshot,
    >::new(bus.clone(), &snapshot_topic, DEFAULT_QUERY_TIMEOUT)?;
    let mut reconciler = Reconciler::new(256);
    let mut local_drops = subscriber.dropped();
    let mut retry_backoff =
        RetryBackoff::new(Duration::from_millis(10), Duration::from_millis(250));

    'query: loop {
        reconciler.begin_query();
        let query = querier.query(state_api::tool::device::SnapshotRequest {
            limit: 1,
            before_sequence: None,
        });
        tokio::pin!(query);
        let snapshot = loop {
            tokio::select! {
                response = &mut query => break response.map_err(|error| anyhow!("tool-telemetry device snapshot query failed: {error}"))?,
                received = subscriber.recv() => {
                    let received = received?;
                    let observed = subscriber.dropped();
                    if observed != local_drops {
                        local_drops = observed;
                        let _ = reconciler.local_drop();
                        prepare_device_requery(&subscriber, &mut local_drops, &mut retry_backoff).await;
                        continue 'query;
                    }
                    let _ = reconciler.follow(DeviceRecordFollow::from(received.body));
                }
            }
        };
        let anchor = Cursor {
            generation: snapshot.cursor.generation.clone(),
            sequence: snapshot.cursor.sequence,
        };
        if snapshot.records.len() > 1
            || snapshot
                .records
                .first()
                .is_some_and(|record| record.sequence != anchor.sequence)
        {
            tracing::warn!("tool-telemetry device snapshot violated its bounded query contract");
            let _ = reconciler.local_drop();
            prepare_device_requery(&subscriber, &mut local_drops, &mut retry_backoff).await;
            continue 'query;
        }
        let anchor_generation = anchor.generation.clone();
        let records = snapshot
            .records
            .into_iter()
            .map(|record| DeviceRecordFollow {
                cursor: Cursor {
                    generation: anchor_generation.clone(),
                    sequence: record.sequence,
                },
                record,
            });
        let outcome = reconciler.install(anchor, records.collect());
        if !apply_device_outcome(telemetry, scope, outcome) {
            prepare_device_requery(&subscriber, &mut local_drops, &mut retry_backoff).await;
            continue 'query;
        }
        retry_backoff.reset();
        loop {
            let received = subscriber.recv().await?;
            let observed = subscriber.dropped();
            if observed != local_drops {
                local_drops = observed;
                let _ = reconciler.local_drop();
                prepare_device_requery(&subscriber, &mut local_drops, &mut retry_backoff).await;
                continue 'query;
            }
            if !apply_device_outcome(
                telemetry,
                scope,
                reconciler.follow(DeviceRecordFollow::from(received.body)),
            ) {
                prepare_device_requery(&subscriber, &mut local_drops, &mut retry_backoff).await;
                continue 'query;
            }
        }
    }
}

#[derive(Debug, Clone)]
struct DeviceRecordFollow {
    cursor: Cursor,
    record: state_api::tool::device::Record,
}

impl From<state_api::tool::device::Follow> for DeviceRecordFollow {
    fn from(follow: state_api::tool::device::Follow) -> Self {
        Self {
            cursor: Cursor {
                generation: follow.cursor.generation,
                sequence: follow.cursor.sequence,
            },
            record: follow.record,
        }
    }
}

impl Sequenced for DeviceRecordFollow {
    fn cursor(&self) -> Cursor {
        self.cursor.clone()
    }
}

fn apply_device_outcome(
    telemetry: &TelemetryBackend,
    scope: &RobotScope,
    outcome: ReconcileOutcome<DeviceRecordFollow>,
) -> bool {
    match outcome {
        ReconcileOutcome::Installed { snapshot, replay } => {
            if let Some(latest) = snapshot
                .into_iter()
                .chain(replay)
                .max_by_key(|item| item.record.sequence)
            {
                telemetry.record_device(scope.clone(), device_sample_from(latest.record));
            }
            true
        }
        ReconcileOutcome::Append(item) => {
            telemetry.record_device(scope.clone(), device_sample_from(item.record));
            true
        }
        ReconcileOutcome::Buffered => true,
        ReconcileOutcome::Requery => false,
    }
}

async fn prepare_device_requery(
    subscriber: &Subscriber<state_api::tool::device::Follow>,
    local_drops: &mut u64,
    backoff: &mut RetryBackoff,
) {
    while subscriber.try_recv().is_some() {}
    *local_drops = subscriber.dropped();
    tokio::time::sleep(backoff.next_delay()).await;
}

/// Reconcile tool-bus's complete bounded snapshot with its live follow feed.
pub(crate) async fn router_metrics_feed_loop(
    bus: &Bus,
    scope: &RobotScope,
    telemetry: &TelemetryBackend,
) -> Result<Infallible> {
    let follow_topic = state_api::topic::client().tool().bus().follow();
    let subscriber =
        Subscriber::<state_api::tool::bus::Follow>::new(bus, &follow_topic, 128).await?;
    let snapshot_topic = state_api::topic::client().tool().bus().snapshot();
    let querier =
        Querier::<state_api::tool::bus::SnapshotRequest, state_api::tool::bus::Snapshot>::new(
            bus.clone(),
            &snapshot_topic,
            DEFAULT_QUERY_TIMEOUT,
        )?;
    let mut reconciler = Reconciler::new(256);
    let mut local_drops = subscriber.dropped();
    let mut retry_backoff =
        RetryBackoff::new(Duration::from_millis(10), Duration::from_millis(250));

    'query: loop {
        reconciler.begin_query();
        let query = querier.query(state_api::tool::bus::SnapshotRequest {});
        tokio::pin!(query);
        loop {
            tokio::select! {
                response = &mut query => {
                    let snapshot = response.map_err(|error| anyhow!("tool-bus snapshot query failed: {error}"))?;
                    let generation = snapshot.cursor.generation.clone();
                    let windows = snapshot.windows.into_iter().map(|window| BusWindowFollow {
                        cursor: Cursor { generation: generation.clone(), sequence: window.sequence },
                        window,
                    }).collect();
                    let outcome = reconciler.install(
                        Cursor { generation: snapshot.cursor.generation, sequence: snapshot.cursor.sequence },
                        windows,
                    );
                    if !apply_bus_outcome(
                        telemetry,
                        scope,
                        outcome,
                        snapshot.current,
                    ) {
                        let _ = reconciler.local_drop();
                        prepare_bus_requery(&subscriber, &mut local_drops, &mut retry_backoff).await;
                        continue 'query;
                    }
                    retry_backoff.reset();
                    break;
                }
                received = subscriber.recv() => {
                    let received = received?;
                    let observed = subscriber.dropped();
                    if observed != local_drops {
                        local_drops = observed;
                        let _ = reconciler.local_drop();
                        prepare_bus_requery(&subscriber, &mut local_drops, &mut retry_backoff).await;
                        continue 'query;
                    }
                    if matches!(reconciler.follow(BusWindowFollow::from(received.body)), ReconcileOutcome::Requery) {
                        prepare_bus_requery(&subscriber, &mut local_drops, &mut retry_backoff).await;
                        continue 'query;
                    }
                }
            }
        }

        loop {
            let received = subscriber.recv().await?;
            let observed = subscriber.dropped();
            if observed != local_drops {
                local_drops = observed;
                let _ = reconciler.local_drop();
                prepare_bus_requery(&subscriber, &mut local_drops, &mut retry_backoff).await;
                continue 'query;
            }
            let outcome = reconciler.follow(BusWindowFollow::from(received.body));
            if matches!(outcome, ReconcileOutcome::Requery)
                || !apply_bus_outcome(telemetry, scope, outcome, None)
            {
                prepare_bus_requery(&subscriber, &mut local_drops, &mut retry_backoff).await;
                continue 'query;
            }
        }
    }
}

async fn prepare_bus_requery(
    subscriber: &Subscriber<state_api::tool::bus::Follow>,
    local_drops: &mut u64,
    backoff: &mut RetryBackoff,
) {
    while subscriber.try_recv().is_some() {}
    *local_drops = subscriber.dropped();
    tokio::time::sleep(backoff.next_delay()).await;
}

#[derive(Debug, Clone)]
struct BusWindowFollow {
    cursor: Cursor,
    window: state_api::tool::bus::Window,
}

impl From<state_api::tool::bus::Follow> for BusWindowFollow {
    fn from(follow: state_api::tool::bus::Follow) -> Self {
        Self {
            cursor: Cursor {
                generation: follow.cursor.generation,
                sequence: follow.cursor.sequence,
            },
            window: follow.window,
        }
    }
}

impl Sequenced for BusWindowFollow {
    fn cursor(&self) -> Cursor {
        self.cursor.clone()
    }
}

fn apply_bus_outcome(
    telemetry: &TelemetryBackend,
    scope: &RobotScope,
    outcome: ReconcileOutcome<BusWindowFollow>,
    current: Option<state_api::tool::bus::Window>,
) -> bool {
    match outcome {
        ReconcileOutcome::Installed { snapshot, replay } => {
            let (samples, current) = timestamp_router_snapshot(
                Instant::now(),
                snapshot
                    .into_iter()
                    .chain(replay)
                    .map(|item| item.window)
                    .collect(),
                current,
            );
            telemetry.install_router(scope.clone(), samples, current);
            true
        }
        ReconcileOutcome::Append(item) => {
            telemetry.record_router_at(
                scope.clone(),
                Instant::now(),
                router_metrics_sample_from(item.window),
            );
            true
        }
        ReconcileOutcome::Buffered => true,
        ReconcileOutcome::Requery => false,
    }
}

fn timestamp_router_snapshot(
    now: Instant,
    windows: Vec<state_api::tool::bus::Window>,
    current: Option<state_api::tool::bus::Window>,
) -> (
    Vec<Timestamped<RouterMetricsSample>>,
    Option<Timestamped<RouterMetricsSample>>,
) {
    let current_duration = current.as_ref().map_or(Duration::ZERO, |window| {
        Duration::from_nanos(window.window_ns)
    });
    let current = current.map(|window| Timestamped::new(router_metrics_sample_from(window), now));
    let mut cursor = now.checked_sub(current_duration).unwrap_or(now);
    let mut timestamped = Vec::with_capacity(windows.len());
    for window in windows.into_iter().rev() {
        let duration = Duration::from_nanos(window.window_ns);
        timestamped.push(Timestamped::new(router_metrics_sample_from(window), cursor));
        cursor = cursor.checked_sub(duration).unwrap_or(cursor);
    }
    timestamped.reverse();
    (timestamped, current)
}

/// Reconcile tool-telemetry's paginated retained runtime history with its live
/// follow feed. The adapter owns transport recovery only; it never changes
/// lifecycle state or samples device resources.
pub(crate) async fn runtime_performance_feed_loop(
    bus: &Bus,
    scope: &RobotScope,
    expected_participant_ids: &[String],
    telemetry: &TelemetryBackend,
    last_capacity_evictions: &mut Option<u64>,
) -> Result<Infallible> {
    let expected = expected_participant_ids
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>();
    let follow_topic = state_api::topic::client().tool().runtime().follow();
    let subscriber =
        Subscriber::<state_api::tool::runtime::Follow>::new(bus, &follow_topic, 512).await?;
    let snapshot_topic = state_api::topic::client().tool().runtime().snapshot();
    let querier = Querier::<
        state_api::tool::runtime::SnapshotRequest,
        state_api::tool::runtime::Snapshot,
    >::new(bus.clone(), &snapshot_topic, DEFAULT_QUERY_TIMEOUT)?;
    let mut reconciler = Reconciler::new(4096);
    let mut local_drops = subscriber.dropped();
    let mut retry_backoff =
        RetryBackoff::new(Duration::from_millis(10), Duration::from_millis(250));

    'query: loop {
        reconciler.begin_query();
        let Some(anchor_snapshot) = query_runtime_snapshot(
            &querier,
            &subscriber,
            &mut reconciler,
            &mut local_drops,
            state_api::tool::runtime::SnapshotRequest {
                participant_id: None,
                limit: 1,
                before_sequence: None,
            },
        )
        .await?
        else {
            prepare_runtime_requery(&subscriber, &mut local_drops, &mut retry_backoff).await;
            continue 'query;
        };
        let Some(anchor) = runtime_anchor_cursor(&anchor_snapshot) else {
            runtime_protocol_violation(&mut reconciler, "invalid anchor page");
            prepare_runtime_requery(&subscriber, &mut local_drops, &mut retry_backoff).await;
            continue 'query;
        };
        let mut capacity_evictions = anchor_snapshot.capacity_evictions;
        let mut records = anchor_snapshot.records;
        for participant_id in &expected {
            let Some(snapshot) = query_runtime_snapshot(
                &querier,
                &subscriber,
                &mut reconciler,
                &mut local_drops,
                state_api::tool::runtime::SnapshotRequest {
                    participant_id: Some(participant_id.clone()),
                    limit: 1,
                    before_sequence: Some(anchor.sequence),
                },
            )
            .await?
            else {
                prepare_runtime_requery(&subscriber, &mut local_drops, &mut retry_backoff).await;
                continue 'query;
            };
            if !runtime_participant_page_is_valid(&snapshot, &anchor, participant_id) {
                runtime_protocol_violation(&mut reconciler, "invalid participant page");
                prepare_runtime_requery(&subscriber, &mut local_drops, &mut retry_backoff).await;
                continue 'query;
            }
            capacity_evictions = capacity_evictions.max(snapshot.capacity_evictions);
            records.extend(snapshot.records);
        }
        let mut latest_by_participant = BTreeMap::new();
        for record in records {
            if expected.contains(&record.participant_id)
                && latest_by_participant
                    .get(&record.participant_id)
                    .is_none_or(|current: &state_api::tool::runtime::Record| {
                        current.sequence < record.sequence
                    })
            {
                latest_by_participant.insert(record.participant_id.clone(), record);
            }
        }
        let snapshot = latest_by_participant
            .into_values()
            .map(|record| RuntimeRecordFollow {
                cursor: Cursor {
                    generation: anchor.generation.clone(),
                    sequence: record.sequence,
                },
                record,
            })
            .collect();
        let status = RuntimeFeedStatus { capacity_evictions };
        let outcome = reconciler.install(anchor, snapshot);
        if !apply_runtime_outcome(telemetry, scope, &expected, outcome, status) {
            let _ = reconciler.local_drop();
            prepare_runtime_requery(&subscriber, &mut local_drops, &mut retry_backoff).await;
            continue 'query;
        }
        disclose_capacity_evictions(last_capacity_evictions, capacity_evictions);
        retry_backoff.reset();

        loop {
            let received = subscriber.recv().await?;
            let observed = subscriber.dropped();
            if observed != local_drops {
                local_drops = observed;
                let _ = reconciler.local_drop();
                prepare_runtime_requery(&subscriber, &mut local_drops, &mut retry_backoff).await;
                continue 'query;
            }
            let outcome = reconciler.follow(RuntimeRecordFollow::from(received.body));
            if matches!(outcome, ReconcileOutcome::Requery)
                || !apply_runtime_outcome(
                    telemetry,
                    scope,
                    &expected,
                    outcome,
                    RuntimeFeedStatus { capacity_evictions },
                )
            {
                prepare_runtime_requery(&subscriber, &mut local_drops, &mut retry_backoff).await;
                continue 'query;
            }
        }
    }
}

async fn query_runtime_snapshot(
    querier: &Querier<
        state_api::tool::runtime::SnapshotRequest,
        state_api::tool::runtime::Snapshot,
    >,
    subscriber: &Subscriber<state_api::tool::runtime::Follow>,
    reconciler: &mut Reconciler<RuntimeRecordFollow>,
    local_drops: &mut u64,
    request: state_api::tool::runtime::SnapshotRequest,
) -> Result<Option<state_api::tool::runtime::Snapshot>> {
    let query = querier.query(request);
    tokio::pin!(query);
    loop {
        tokio::select! {
            response = &mut query => {
                return response
                    .map(Some)
                    .map_err(|error| anyhow!("tool-telemetry runtime snapshot query failed: {error}"));
            }
            received = subscriber.recv() => {
                let received = received?;
                let observed = subscriber.dropped();
                if observed != *local_drops {
                    *local_drops = observed;
                    let _ = reconciler.local_drop();
                    return Ok(None);
                }
                let _ = reconciler.follow(RuntimeRecordFollow::from(received.body));
            }
        }
    }
}

fn runtime_protocol_violation(
    reconciler: &mut Reconciler<RuntimeRecordFollow>,
    reason: &'static str,
) {
    tracing::warn!(
        reason,
        "tool-telemetry runtime snapshot violated its bounded query contract"
    );
    let _ = reconciler.local_drop();
}

fn runtime_anchor_cursor(snapshot: &state_api::tool::runtime::Snapshot) -> Option<Cursor> {
    let cursor = Cursor {
        generation: snapshot.cursor.generation.clone(),
        sequence: snapshot.cursor.sequence,
    };
    (snapshot.records.len() <= 1
        && snapshot
            .records
            .first()
            .is_none_or(|record| record.sequence == cursor.sequence))
    .then_some(cursor)
}

fn runtime_participant_page_is_valid(
    snapshot: &state_api::tool::runtime::Snapshot,
    anchor: &Cursor,
    participant_id: &str,
) -> bool {
    snapshot.cursor.generation == anchor.generation
        && snapshot.cursor.sequence >= anchor.sequence
        && snapshot.records.len() <= 1
        && snapshot.records.first().is_none_or(|record| {
            record.participant_id == participant_id && record.sequence < anchor.sequence
        })
}

fn disclose_capacity_evictions(previous: &mut Option<u64>, current: u64) {
    let delta = capacity_eviction_delta(*previous, current);
    if delta > 0 {
        tracing::warn!(
            capacity_evictions = current,
            new_capacity_evictions = delta,
            "tool-telemetry runtime history was shortened by its memory bound"
        );
    }
    *previous = Some(current);
}

fn capacity_eviction_delta(previous: Option<u64>, current: u64) -> u64 {
    previous.map_or(current, |prior| current.saturating_sub(prior))
}

async fn prepare_runtime_requery(
    subscriber: &Subscriber<state_api::tool::runtime::Follow>,
    local_drops: &mut u64,
    backoff: &mut RetryBackoff,
) {
    while subscriber.try_recv().is_some() {}
    *local_drops = subscriber.dropped();
    tokio::time::sleep(backoff.next_delay()).await;
}

#[derive(Debug, Clone)]
struct RuntimeRecordFollow {
    cursor: Cursor,
    record: state_api::tool::runtime::Record,
}

impl From<state_api::tool::runtime::Follow> for RuntimeRecordFollow {
    fn from(follow: state_api::tool::runtime::Follow) -> Self {
        Self {
            cursor: Cursor {
                generation: follow.cursor.generation,
                sequence: follow.cursor.sequence,
            },
            record: follow.record,
        }
    }
}

impl Sequenced for RuntimeRecordFollow {
    fn cursor(&self) -> Cursor {
        self.cursor.clone()
    }
}

fn apply_runtime_outcome(
    telemetry: &TelemetryBackend,
    scope: &RobotScope,
    expected: &BTreeSet<String>,
    outcome: ReconcileOutcome<RuntimeRecordFollow>,
    status: RuntimeFeedStatus,
) -> bool {
    match outcome {
        ReconcileOutcome::Installed { snapshot, replay } => {
            telemetry.install_runtimes(
                scope.clone(),
                snapshot
                    .into_iter()
                    .chain(replay)
                    .filter(|item| expected.contains(&item.record.participant_id))
                    .map(|item| runtime_record_from(item.record))
                    .collect(),
                status,
            );
            true
        }
        ReconcileOutcome::Append(item) => {
            if expected.contains(&item.record.participant_id) {
                telemetry.record_runtime(scope.clone(), runtime_record_from(item.record));
            }
            true
        }
        ReconcileOutcome::Buffered => true,
        ReconcileOutcome::Requery => false,
    }
}

/// Subscribe v0_1::joypad::Devices and own the Select, SetEnabled, and Rescan
/// publishers. This loop publishes typed port commands, and the next Devices
/// receive is the authoritative acknowledgement.
pub(crate) async fn joypad_devices_feed_loop(
    bus: Bus,
    telemetry: &TelemetryBackend,
    command_rx: &mut mpsc::Receiver<crate::ports::input::InputCommand>,
) -> Result<()> {
    {
        let devices_topic = Topic::<Subscribe<api::joypad::Devices>>::new_static(
            <api::joypad::Devices as ContractBody>::TOPIC,
        );
        let devices_subscriber =
            Subscriber::<api::joypad::Devices>::new(&bus, &devices_topic, 32).await?;
        let select_topic = Topic::<Publish<api::joypad::Select>>::new_static(
            <api::joypad::Select as ContractBody>::TOPIC,
        );
        let select_publisher =
            CommandPublisher::<api::joypad::Select>::new(bus.clone(), &select_topic)?;
        let enabled_topic = Topic::<Publish<api::joypad::SetEnabled>>::new_static(
            <api::joypad::SetEnabled as ContractBody>::TOPIC,
        );
        let enabled_publisher =
            CommandPublisher::<api::joypad::SetEnabled>::new(bus.clone(), &enabled_topic)?;
        let rescan_topic = Topic::<Publish<api::joypad::Rescan>>::new_static(
            <api::joypad::Rescan as ContractBody>::TOPIC,
        );
        let rescan_publisher =
            CommandPublisher::<api::joypad::Rescan>::new(bus.clone(), &rescan_topic)?;
        loop {
            tokio::select! {
                received = devices_subscriber.recv() => {
                    let received = received?;
                    telemetry.record_joypad(joypad_devices_sample_from(received.body));
                }
                command = command_rx.recv() => {
                    match command {
                        Some(crate::ports::input::InputCommand::Select(id)) => {
                            if let Err(error) = select_publisher.send(api::joypad::Select { id }) {
                                tracing::warn!("joypad select publish failed: {error:#}");
                            }
                        }
                        Some(crate::ports::input::InputCommand::SetEnabled(enabled)) => {
                            if let Err(error) = enabled_publisher.send(api::joypad::SetEnabled { enabled }) {
                                tracing::warn!("joypad enable publish failed: {error:#}");
                            }
                        }
                        Some(crate::ports::input::InputCommand::Rescan) => {
                            if let Err(error) = rescan_publisher.send(api::joypad::Rescan {}) {
                                tracing::warn!("joypad rescan publish failed: {error:#}");
                            }
                        }
                        // Closing the typed port ends this source cleanly.
                        None => return Ok(()),
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runtime_record(sequence: u64, participant_id: &str) -> state_api::tool::runtime::Record {
        state_api::tool::runtime::Record {
            sequence,
            participant_id: participant_id.to_string(),
            truncated: 0,
            window_ns: 1,
            step: None,
            topics: Vec::new(),
            overflow: None,
        }
    }

    fn runtime_snapshot(
        generation: &str,
        sequence: u64,
        records: Vec<state_api::tool::runtime::Record>,
    ) -> state_api::tool::runtime::Snapshot {
        state_api::tool::runtime::Snapshot {
            cursor: state_api::tool::Cursor {
                generation: generation.to_string(),
                sequence,
            },
            records,
            capacity_evictions: 0,
            next_before_sequence: Some(1),
        }
    }

    #[test]
    fn runtime_query_pages_reject_hostile_anchor_and_filtered_responses() {
        let anchor = runtime_anchor_cursor(&runtime_snapshot(
            "g",
            10,
            vec![runtime_record(10, "drive")],
        ))
        .expect("valid anchor");
        assert!(
            runtime_anchor_cursor(&runtime_snapshot("g", 10, vec![runtime_record(9, "drive")],))
                .is_none()
        );
        assert!(
            runtime_anchor_cursor(&runtime_snapshot(
                "g",
                10,
                vec![runtime_record(10, "drive"), runtime_record(10, "camera")],
            ))
            .is_none()
        );
        assert!(runtime_participant_page_is_valid(
            &runtime_snapshot("g", 11, vec![runtime_record(8, "drive")]),
            &anchor,
            "drive",
        ));
        for invalid in [
            runtime_snapshot("other", 11, vec![runtime_record(8, "drive")]),
            runtime_snapshot("g", 9, vec![runtime_record(8, "drive")]),
            runtime_snapshot("g", 11, vec![runtime_record(10, "drive")]),
            runtime_snapshot("g", 11, vec![runtime_record(8, "camera")]),
            runtime_snapshot(
                "g",
                11,
                vec![runtime_record(8, "drive"), runtime_record(7, "drive")],
            ),
        ] {
            assert!(!runtime_participant_page_is_valid(
                &invalid, &anchor, "drive"
            ));
        }
    }

    #[test]
    fn capacity_evictions_disclose_only_positive_cumulative_edges() {
        assert_eq!(capacity_eviction_delta(None, 0), 0);
        assert_eq!(capacity_eviction_delta(None, 4), 4);
        assert_eq!(capacity_eviction_delta(Some(4), 4), 0);
        assert_eq!(capacity_eviction_delta(Some(4), 7), 3);
        assert_eq!(capacity_eviction_delta(Some(7), 2), 0);
    }

    #[test]
    fn retained_router_windows_reconstruct_distinct_end_times() {
        fn window(sequence: u64, window_ns: u64) -> state_api::tool::bus::Window {
            state_api::tool::bus::Window {
                sequence,
                topics: Vec::new(),
                throughput_msg_s: sequence as f32,
                window_ns,
            }
        }
        let now = Instant::now();
        let (history, current) = timestamp_router_snapshot(
            now,
            vec![window(1, 1_000_000_000), window(2, 2_000_000_000)],
            Some(window(3, 500_000_000)),
        );
        assert_eq!(history[1].received_at, now - Duration::from_millis(500));
        assert_eq!(history[0].received_at, now - Duration::from_millis(2_500));
        assert_eq!(current.unwrap().received_at, now);
    }

    #[test]
    fn router_overflow_and_remote_labels_are_preserved_safely() {
        let metric = topic_metric_from(state_api::tool::bus::TopicMetric {
            topic: String::new(),
            from_participant: String::new(),
            ingress_rate_hz: 7.0,
            count: 11,
        });
        assert_eq!(metric.topic, "Other/unobserved traffic");
        assert_eq!(metric.from_participant, "multiple");
        assert!(metric.aggregate_overflow);

        let metric = topic_metric_from(state_api::tool::bus::TopicMetric {
            topic: "v1/drive\u{1b}[31m/state\u{e0021}".to_string(),
            from_participant: "drive\nspoof".to_string(),
            ingress_rate_hz: 1.0,
            count: 1,
        });
        assert_eq!(metric.topic, "v1/drive/state ");
        assert_eq!(metric.from_participant, "drive spoof");
    }

    #[test]
    fn oversized_remote_telemetry_is_bounded_and_disclosed() {
        let router = router_metrics_sample_from(state_api::tool::bus::Window {
            sequence: 1,
            topics: (0..(MAX_ROUTER_TOPICS + 20))
                .map(|index| state_api::tool::bus::TopicMetric {
                    topic: format!("{}-{index}", "t".repeat(MAX_REMOTE_TEXT_CHARS * 2)),
                    from_participant: "p".repeat(MAX_REMOTE_TEXT_CHARS * 2),
                    ingress_rate_hz: 1.0,
                    count: 1,
                })
                .collect(),
            throughput_msg_s: 1.0,
            window_ns: 1,
        });
        assert_eq!(router.topics.len(), MAX_ROUTER_TOPICS);
        assert_eq!(router.topics_truncated, 21);
        let overflow = router
            .topics
            .iter()
            .find(|metric| metric.aggregate_overflow)
            .expect("overflow aggregate");
        assert_eq!(overflow.ingress_rate_hz, 21.0);
        assert_eq!(overflow.count, 21);

        let joypad = joypad_devices_sample_from(api::joypad::Devices {
            available: (0..(MAX_JOYPAD_DEVICES + 7))
                .map(|index| api::joypad::Device {
                    id: format!("{}-{index}", "i".repeat(MAX_REMOTE_TEXT_CHARS * 2)),
                    name: "n".repeat(MAX_REMOTE_TEXT_CHARS * 2),
                    status: api::joypad::DeviceStatus::Ready,
                })
                .collect(),
            selected: None,
            enabled: false,
            unavailable_reason: None,
            last_error: None,
        });
        assert_eq!(joypad.available.len(), MAX_JOYPAD_DEVICES);
        assert_eq!(joypad.devices_truncated, 7);
    }

    #[test]
    fn local_router_truncation_folds_into_existing_overflow() {
        let mut topics = vec![state_api::tool::bus::TopicMetric {
            topic: String::new(),
            from_participant: String::new(),
            ingress_rate_hz: 100.0,
            count: 100,
        }];
        topics.extend((0..(MAX_ROUTER_TOPICS + 20)).map(|index| {
            state_api::tool::bus::TopicMetric {
                topic: format!("v1/topic/{index}"),
                from_participant: format!("producer-{index}"),
                ingress_rate_hz: 1.0,
                count: 1,
            }
        }));
        let router = router_metrics_sample_from(state_api::tool::bus::Window {
            sequence: 1,
            topics,
            throughput_msg_s: 0.0,
            window_ns: 1,
        });
        let overflow = router
            .topics
            .iter()
            .find(|metric| metric.aggregate_overflow)
            .expect("router sentinel must remain visible");
        assert_eq!(router.topics_truncated, 22);
        assert_eq!(overflow.ingress_rate_hz, 122.0);
        assert_eq!(overflow.count, 122);
    }
}
