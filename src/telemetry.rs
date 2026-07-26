//! Live telemetry feed for the TUI (CLI-UX Phase 3/4): background bus
//! subscribers for the framework train's tool telemetry and joypad contracts,
//! mirroring `supervisor::start_bus_log_subscriber`/
//! `start_liveliness_observer`'s "observe, update shared
//! snapshot" pattern.
//!
//! Kept deliberately separate from `supervisor::BoardBackend`/`BoardSnapshot`
//! (participant board state, persisted to the state file): telemetry never
//! reaches either of those, only the live TUI reads it
//! (`TelemetryBackend::snapshot`), so it can carry
//! whatever shape is convenient for rendering without touching the persisted
//! board contract.

use std::collections::{BTreeMap, BTreeSet};
use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use phoxal::bus::{
    CommandPublisher, ContractBody, DEFAULT_QUERY_TIMEOUT, Publish, Querier, Subscribe, Subscriber,
    Topic,
};
use phoxal::raw::{Bus, BusConfig};
use phoxal_api::v0_1 as api;
use phoxal_api::v0_1 as state_api;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

use phoxal_cli_core::session::reconcile::{
    Cursor, ReconcileOutcome, Reconciler, RetryBackoff, Sequenced,
};
use phoxal_cli_core::session::stores::log::sanitize_terminal_text;
use phoxal_cli_core::session::stores::telemetry::{RobotScope, TelemetryStore, Timestamped};
use phoxal_cli_core::session::telemetry::{
    ClockObservation, DeviceDiskSample, DeviceSample, JoypadCommand, JoypadDevice,
    JoypadDeviceStatus, JoypadDevicesSample, RouterMetricsSample, RuntimeBufferKind,
    RuntimeDirection, RuntimeFeedStatus, RuntimePerformanceSample, RuntimeStepSample,
    RuntimeTopicSample, TelemetrySnapshot, TopicMetric,
};

const MAX_DEVICE_DISKS: usize = 32;
const MAX_ROUTER_TOPICS: usize = 256;
const MAX_JOYPAD_DEVICES: usize = 64;
const MAX_REMOTE_TEXT_CHARS: usize = 256;
const MAX_EXPECTED_RUNTIME_PARTICIPANTS: usize = 1024;

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
/// AUTHORITATIVE selection (the tool's own ack), never a local UI guess; see
/// `tui::state::AppState::input_cursor` for the separate, purely local list cursor.
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

/// A snapshot of the selected robot's live telemetry feeds, cloned once per TUI redraw
/// (mirrors `BoardBackend::snapshot`). Every latest-value field is a
/// [`Timestamped`] carrying the [`Instant`] the underlying sample was
/// actually RECEIVED off the bus (recorded by `TelemetryBackend::record_*`
/// at the moment a feed task observes it, never re-stamped on later
/// redraws), so a renderer can ask [`Timestamped::is_stale`] instead of
/// trusting a long-cached value as if it were still live. See the
/// `stores::telemetry_store` module documentation for the store contract.
/// Live-telemetry state shared between the background feed tasks
/// (`start_device_feed` etc.) and the TUI's redraw path
/// (`TuiDisplay::redraw`/`render::draw`). Cheap to clone (an `Arc` handle);
/// every feed task and the TUI hold their own clone.
///
/// Backed by [`TelemetryStore`]: every `record_*` call below
/// stamps the sample with `Instant::now()` AT THE MOMENT the feed task
/// received it, not when a later redraw happens to observe it - that
/// receive-time timestamp is what makes [`TelemetrySnapshot`]'s freshness
/// checks meaningful.
#[derive(Debug, Clone, Default)]
pub struct TelemetryBackend {
    inner: Arc<Mutex<TelemetryStore>>,
    clock_rx: Arc<Mutex<Option<watch::Receiver<ClockObservation>>>>,
    joypad_command_tx: Arc<Mutex<Option<mpsc::Sender<JoypadCommand>>>>,
}

/// A generous bound for a user-driven command channel (one send per
/// keypress in Input, never a hot loop): [`JoypadCommand`]s queue
/// here between redraws, so this only needs to absorb a rapid burst of
/// key presses, not hold unbounded history.
const JOYPAD_COMMAND_CHANNEL_CAPACITY: usize = 16;

impl TelemetryBackend {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_clock_feed(&self, rx: watch::Receiver<ClockObservation>) {
        *self.clock_rx.lock().expect("clock_rx mutex poisoned") = Some(rx);
    }

    fn set_joypad_command_sender(&self, tx: mpsc::Sender<JoypadCommand>) {
        *self
            .joypad_command_tx
            .lock()
            .expect("joypad_command_tx mutex poisoned") = Some(tx);
    }

    /// Publish a joypad Select, SetEnabled, or Rescan command from the TUI's
    /// Input page. Never blocks the terminal input path. An absent, closed, or
    /// overloaded feed is reported through tracing so the failure appears in
    /// Logs instead of being duplicated as persistent Input-panel state.
    pub fn send_joypad_command(&self, command: JoypadCommand) {
        let sender = self
            .joypad_command_tx
            .lock()
            .expect("joypad_command_tx mutex poisoned")
            .clone();
        let Some(sender) = sender else {
            tracing::warn!(?command, "joypad action rejected: tool feed is unavailable");
            return;
        };
        match sender.try_send(command) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(command)) => {
                tracing::warn!(?command, "joypad action rejected: tool feed is busy");
            }
            Err(mpsc::error::TrySendError::Closed(command)) => {
                tracing::warn!(?command, "joypad action rejected: tool feed stopped");
            }
        }
    }

    #[must_use]
    pub fn snapshot(&self, scope: &RobotScope) -> TelemetrySnapshot {
        let store = self.inner.lock().expect("telemetry mutex poisoned");
        let mut snapshot = TelemetrySnapshot {
            scope: Some(scope.clone()),
            clock: None,
            device: store.device(scope).cloned(),
            router: store.router(scope).cloned(),
            router_throughput_history: store.router_throughput_history(scope).collect(),
            runtimes: store.runtimes(scope),
            runtime_status: store.runtime_status(scope),
            joypad: store.joypad().cloned(),
            motion: store.motion().cloned(),
        };
        drop(store);
        if let Some(rx) = &*self.clock_rx.lock().expect("clock_rx mutex poisoned") {
            let observation = rx.borrow();
            snapshot.clock = observation
                .latest
                .zip(observation.received_at)
                .map(|(value, received_at)| Timestamped { value, received_at });
        }
        snapshot
    }

    fn record_device(&self, scope: RobotScope, sample: DeviceSample) {
        self.inner
            .lock()
            .expect("telemetry mutex poisoned")
            .record_device(scope, Instant::now(), sample);
    }

    fn record_router_at(
        &self,
        scope: RobotScope,
        received_at: Instant,
        sample: RouterMetricsSample,
    ) {
        self.inner
            .lock()
            .expect("telemetry mutex poisoned")
            .record_router(scope, received_at, sample);
    }

    fn install_router(
        &self,
        scope: RobotScope,
        samples: Vec<Timestamped<RouterMetricsSample>>,
        current: Option<Timestamped<RouterMetricsSample>>,
    ) {
        self.inner
            .lock()
            .expect("telemetry mutex poisoned")
            .install_router_history(scope, samples, current);
    }

    fn install_runtimes(
        &self,
        scope: RobotScope,
        samples: Vec<RuntimePerformanceSample>,
        status: RuntimeFeedStatus,
    ) {
        self.inner
            .lock()
            .expect("telemetry mutex poisoned")
            .install_runtime_history(scope, Instant::now(), samples, status);
    }

    fn record_runtime(&self, scope: RobotScope, sample: RuntimePerformanceSample) {
        self.inner
            .lock()
            .expect("telemetry mutex poisoned")
            .record_runtime(scope, Instant::now(), sample);
    }

    fn record_joypad(&self, sample: JoypadDevicesSample) {
        self.inner
            .lock()
            .expect("telemetry mutex poisoned")
            .record_joypad(Instant::now(), sample);
    }

    fn record_motion(&self, sample: state_api::motion::State) {
        self.inner
            .lock()
            .expect("telemetry mutex poisoned")
            .record_motion(Instant::now(), sample);
    }
}

pub fn start_control_state_feed(
    namespace: String,
    robot_id: String,
    connect: String,
    execution: phoxal::bus::ExecutionId,
    telemetry: TelemetryBackend,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            if let Err(error) = control_state_feed_loop(
                namespace.clone(),
                robot_id.clone(),
                connect.clone(),
                execution,
                &telemetry,
            )
            .await
            {
                tracing::debug!("control-state feed waiting for router: {error:#}");
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    })
}

async fn control_state_feed_loop(
    namespace: String,
    robot_id: String,
    connect: String,
    execution: phoxal::bus::ExecutionId,
    telemetry: &TelemetryBackend,
) -> Result<()> {
    let bus = Bus::open(BusConfig {
        namespace,
        robot_id,
        participant: "phoxal-cli-control-state".to_string(),
        execution,
        producer: phoxal::bus::ProducerId::mint(),
        connect_endpoints: vec![connect],
    })
    .await?;
    let result = async {
        let motion_topic = Topic::<Subscribe<state_api::motion::State>>::new_static(
            <state_api::motion::State as ContractBody>::TOPIC,
        );
        let motion = Subscriber::new(&bus, &motion_topic, 32).await?;
        loop {
            telemetry.record_motion(motion.recv().await?.body);
        }
    }
    .await;
    close_feed_bus(&bus, "control-state").await;
    result
}

/// Reconcile one robot root's retained device observation with its live
/// follow feed. Device totals retain both their project/deployment identity
/// and robot-root attribution and are never attributed to a runtime.
pub fn start_device_feed(
    namespace: String,
    robot_id: String,
    connect: String,
    execution: phoxal::bus::ExecutionId,
    telemetry: TelemetryBackend,
    mut recovery_epochs: watch::Receiver<u64>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let scope = RobotScope {
            namespace: namespace.clone(),
            robot_id: robot_id.clone(),
        };
        loop {
            let bus = match Bus::open(BusConfig {
                namespace: namespace.clone(),
                robot_id: robot_id.clone(),
                participant: "phoxal-cli-tool-device-consumer".to_string(),
                execution,
                producer: phoxal::bus::ProducerId::mint(),
                connect_endpoints: vec![connect.clone()],
            })
            .await
            {
                Ok(bus) => bus,
                Err(error) => {
                    tracing::debug!("device telemetry feed waiting for router: {error}");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    continue;
                }
            };
            let feed = device_feed_loop(&bus, &scope, &telemetry);
            tokio::pin!(feed);
            let result = tokio::select! {
                result = &mut feed => Some(result),
                changed = recovery_epochs.changed() => {
                    if changed.is_err() { break; }
                    tracing::debug!(
                        recovery_epoch = *recovery_epochs.borrow_and_update(),
                        "recreating device snapshot/follow transport after graph recovery"
                    );
                    None
                }
            };
            close_feed_bus(&bus, "tool-telemetry/device").await;
            if let Some(result) = result {
                let error =
                    result.expect_err("device telemetry feed loop is intentionally endless");
                tracing::debug!("device telemetry feed waiting for router: {error:#}");
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    })
}

async fn device_feed_loop(
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
pub fn start_router_metrics_feed(
    namespace: String,
    robot_id: String,
    connect: String,
    execution: phoxal::bus::ExecutionId,
    telemetry: TelemetryBackend,
    mut recovery_epochs: watch::Receiver<u64>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let scope = RobotScope {
            namespace: namespace.clone(),
            robot_id: robot_id.clone(),
        };
        loop {
            let bus = match Bus::open(BusConfig {
                namespace: namespace.clone(),
                robot_id: robot_id.clone(),
                participant: "phoxal-cli-tool-bus-consumer".to_string(),
                execution,
                producer: phoxal::bus::ProducerId::mint(),
                connect_endpoints: vec![connect.clone()],
            })
            .await
            {
                Ok(bus) => bus,
                Err(error) => {
                    tracing::debug!("router metrics feed waiting for router: {error}");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    continue;
                }
            };
            let feed = router_metrics_feed_loop(&bus, &scope, &telemetry);
            tokio::pin!(feed);
            let result = tokio::select! {
                result = &mut feed => Some(result),
                changed = recovery_epochs.changed() => {
                    if changed.is_err() {
                        break;
                    }
                    tracing::debug!(
                        recovery_epoch = *recovery_epochs.borrow_and_update(),
                        "recreating tool-bus snapshot/follow transport after graph recovery"
                    );
                    None
                }
            };
            close_feed_bus(&bus, "tool-bus").await;
            match result {
                None => continue,
                Some(result) => {
                    let error =
                        result.expect_err("router metrics feed loop is intentionally endless");
                    tracing::debug!("router metrics feed waiting for router: {error:#}");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
        }
    })
}

async fn router_metrics_feed_loop(
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
pub fn start_runtime_performance_feed(
    namespace: String,
    robot_id: String,
    expected_participant_ids: Vec<String>,
    connect: String,
    execution: phoxal::bus::ExecutionId,
    telemetry: TelemetryBackend,
    mut recovery_epochs: watch::Receiver<u64>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let expected_count = expected_runtime_participant_count(&expected_participant_ids);
        if expected_count > MAX_EXPECTED_RUNTIME_PARTICIPANTS {
            tracing::error!(
                expected_count,
                limit = MAX_EXPECTED_RUNTIME_PARTICIPANTS,
                "runtime telemetry disabled: configured participant set exceeds the static limit"
            );
            return;
        }
        let scope = RobotScope {
            namespace: namespace.clone(),
            robot_id: robot_id.clone(),
        };
        let mut last_capacity_evictions = None;
        loop {
            let bus = match Bus::open(BusConfig {
                namespace: namespace.clone(),
                robot_id: robot_id.clone(),
                participant: "phoxal-cli-tool-telemetry-consumer".to_string(),
                execution,
                producer: phoxal::bus::ProducerId::mint(),
                connect_endpoints: vec![connect.clone()],
            })
            .await
            {
                Ok(bus) => bus,
                Err(error) => {
                    tracing::debug!("runtime telemetry feed waiting for router: {error}");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    continue;
                }
            };
            let feed = runtime_performance_feed_loop(
                &bus,
                &scope,
                &expected_participant_ids,
                &telemetry,
                &mut last_capacity_evictions,
            );
            tokio::pin!(feed);
            let result = tokio::select! {
                result = &mut feed => Some(result),
                changed = recovery_epochs.changed() => {
                    if changed.is_err() { break; }
                    tracing::debug!(
                        recovery_epoch = *recovery_epochs.borrow_and_update(),
                        "recreating tool-telemetry snapshot/follow transport after graph recovery"
                    );
                    None
                }
            };
            close_feed_bus(&bus, "tool-telemetry/runtime").await;
            match result {
                None => continue,
                Some(result) => {
                    let error =
                        result.expect_err("runtime telemetry feed loop is intentionally endless");
                    tracing::debug!("runtime telemetry feed waiting for router: {error:#}");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
        }
    })
}

fn expected_runtime_participant_count(participant_ids: &[String]) -> usize {
    participant_ids.iter().collect::<BTreeSet<_>>().len()
}

async fn runtime_performance_feed_loop(
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
/// publishers. The TUI's Input page sends commands through DisplayAction;
/// this loop publishes, and the next Devices receive is the authoritative
/// acknowledgement. The command sender is installed on telemetry immediately,
/// before the feed connects, so a command sent while the bus reconnects is
/// queued. The returned handle owns both subscription and command publishing.
pub fn start_joypad_devices_feed(
    namespace: String,
    robot_id: String,
    connect: String,
    execution: phoxal::bus::ExecutionId,
    telemetry: TelemetryBackend,
) -> JoinHandle<()> {
    let (command_tx, mut command_rx) = mpsc::channel(JOYPAD_COMMAND_CHANNEL_CAPACITY);
    telemetry.set_joypad_command_sender(command_tx);
    tokio::spawn(async move {
        loop {
            match joypad_devices_feed_loop(
                namespace.clone(),
                robot_id.clone(),
                connect.clone(),
                execution,
                &telemetry,
                &mut command_rx,
            )
            .await
            {
                Ok(()) => break,
                Err(error) => {
                    tracing::debug!("joypad devices feed waiting for router: {error:#}");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
        }
    })
}

async fn joypad_devices_feed_loop(
    namespace: String,
    robot_id: String,
    connect: String,
    execution: phoxal::bus::ExecutionId,
    telemetry: &TelemetryBackend,
    command_rx: &mut mpsc::Receiver<JoypadCommand>,
) -> Result<()> {
    let bus = Bus::open(BusConfig {
        namespace,
        robot_id,
        participant: "phoxal-cli-telemetry-joypad".to_string(),
        execution,
        producer: phoxal::bus::ProducerId::mint(),
        connect_endpoints: vec![connect],
    })
    .await
    .map_err(|error| anyhow!("failed to open bus joypad/devices subscription: {error}"))?;
    let result = async {
        let devices_topic = Topic::<Subscribe<api::joypad::Devices>>::new_static(
            <api::joypad::Devices as ContractBody>::TOPIC,
        );
        let devices_subscriber =
            Subscriber::<api::joypad::Devices>::new(&bus, &devices_topic, 32).await?;
        let select_topic = Topic::<Publish<api::joypad::Select>>::new_static(
            <api::joypad::Select as ContractBody>::TOPIC,
        );
        let select_publisher = CommandPublisher::<api::joypad::Select>::new(bus.clone(), &select_topic)?;
        let enabled_topic = Topic::<Publish<api::joypad::SetEnabled>>::new_static(
            <api::joypad::SetEnabled as ContractBody>::TOPIC,
        );
        let enabled_publisher =
            CommandPublisher::<api::joypad::SetEnabled>::new(bus.clone(), &enabled_topic)?;
        let rescan_topic = Topic::<Publish<api::joypad::Rescan>>::new_static(
            <api::joypad::Rescan as ContractBody>::TOPIC,
        );
        let rescan_publisher = CommandPublisher::<api::joypad::Rescan>::new(bus.clone(), &rescan_topic)?;
        loop {
            tokio::select! {
                received = devices_subscriber.recv() => {
                    let received = received?;
                    telemetry.record_joypad(joypad_devices_sample_from(received.body));
                }
                command = command_rx.recv() => {
                    match command {
                        Some(JoypadCommand::Select(id)) => {
                            if let Err(error) = select_publisher.send(api::joypad::Select { id }) {
                                tracing::warn!("joypad select publish failed: {error:#}");
                            }
                        }
                        Some(JoypadCommand::SetEnabled(enabled)) => {
                            if let Err(error) = enabled_publisher.send(api::joypad::SetEnabled { enabled }) {
                                tracing::warn!("joypad enable publish failed: {error:#}");
                            }
                        }
                        Some(JoypadCommand::Rescan) => {
                            if let Err(error) = rescan_publisher.send(api::joypad::Rescan {}) {
                                tracing::warn!("joypad rescan publish failed: {error:#}");
                            }
                        }
                        // The `TelemetryBackend` handle (and its command sender)
                        // outlives every feed task in practice, so this arm is
                        // untaken outside tests - kept as a clean exit rather
                        // than an unreachable! so a future caller that DOES drop
                        // the backend mid-session degrades gracefully instead of
                        // panicking.
                        None => return Ok(()),
                    }
                }
            }
        }
    }
    .await;
    close_feed_bus(&bus, "joypad/devices").await;
    result
}

async fn close_feed_bus(bus: &Bus, feed: &str) {
    if let Err(error) = bus.close().await {
        tracing::debug!(feed, error = %error, "telemetry feed bus close failed");
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
    fn runtime_participant_limit_counts_unique_static_configuration() {
        let within_limit = (0..MAX_EXPECTED_RUNTIME_PARTICIPANTS)
            .map(|index| format!("participant-{index}"))
            .collect::<Vec<_>>();
        assert_eq!(
            expected_runtime_participant_count(&within_limit),
            MAX_EXPECTED_RUNTIME_PARTICIPANTS
        );
        let mut over_limit = within_limit;
        over_limit.push("one-too-many".to_string());
        assert_eq!(
            expected_runtime_participant_count(&over_limit),
            MAX_EXPECTED_RUNTIME_PARTICIPANTS + 1
        );
        over_limit.push("one-too-many".to_string());
        assert_eq!(
            expected_runtime_participant_count(&over_limit),
            MAX_EXPECTED_RUNTIME_PARTICIPANTS + 1
        );
    }

    #[test]
    fn snapshot_is_empty_by_default_graceful_absence() {
        let telemetry = TelemetryBackend::new();
        let snapshot = telemetry.snapshot(&scope("r1"));
        assert!(snapshot.device.is_none());
        assert!(snapshot.clock.is_none());
        assert!(snapshot.router.is_none());
        assert!(snapshot.joypad.is_none());
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
    fn record_device_is_scoped_to_its_robot_root() {
        let telemetry = TelemetryBackend::new();
        telemetry.record_device(
            scope("r1"),
            DeviceSample {
                cpu_pct: Some(42.0),
                ram_used_bytes: Some(100),
                ram_total_bytes: Some(200),
                load_1m: Some(0.5),
                window_ns: 1_000_000_000,
                ..DeviceSample::default()
            },
        );
        let snapshot = telemetry.snapshot(&scope("r1"));
        assert_eq!(
            snapshot.device.and_then(|device| device.value.cpu_pct),
            Some(42.0)
        );
        assert!(telemetry.snapshot(&scope("r2")).device.is_none());
    }

    #[test]
    fn device_reconciliation_installs_the_latest_identity_derived_sample() {
        fn item(sequence: u64, cpu_pct: f32) -> DeviceRecordFollow {
            DeviceRecordFollow {
                cursor: Cursor {
                    generation: "generation-a".to_string(),
                    sequence,
                },
                record: state_api::tool::device::Record {
                    sequence,
                    sample: state_api::tool::device::Sample {
                        cpu_pct: Some(cpu_pct),
                        ram_used_bytes: None,
                        ram_total_bytes: None,
                        swap_used_bytes: None,
                        swap_total_bytes: None,
                        load_1m: None,
                        load_5m: None,
                        load_15m: None,
                        uptime_s: None,
                        disks: None,
                        window_ns: 1,
                    },
                    truncated: 0,
                },
            }
        }

        let telemetry = TelemetryBackend::new();
        let target = scope("r1");
        assert!(apply_device_outcome(
            &telemetry,
            &target,
            ReconcileOutcome::Installed {
                snapshot: vec![item(1, 10.0)],
                replay: vec![item(3, 30.0)],
            },
        ));
        assert_eq!(
            telemetry
                .snapshot(&target)
                .device
                .map(|device| device.value.cpu_pct),
            Some(Some(30.0))
        );
    }

    #[test]
    fn router_overflow_sentinel_is_presented_as_an_aggregate_row() {
        let metric = topic_metric_from(state_api::tool::bus::TopicMetric {
            topic: String::new(),
            from_participant: String::new(),
            ingress_rate_hz: 7.0,
            count: 11,
        });
        assert_eq!(metric.topic, "Other/unobserved traffic");
        assert_eq!(metric.from_participant, "multiple");
        assert_eq!(metric.ingress_rate_hz, 7.0);
        assert_eq!(metric.count, 11);
        assert!(metric.aggregate_overflow);
    }

    #[test]
    fn remote_router_and_joypad_labels_are_sanitized_at_ingress() {
        let metric = topic_metric_from(state_api::tool::bus::TopicMetric {
            topic: "v1/drive\u{1b}[31m/state".to_string(),
            from_participant: "drive\nspoof".to_string(),
            ingress_rate_hz: 1.0,
            count: 1,
        });
        assert_eq!(metric.topic, "v1/drive/state");
        assert_eq!(metric.from_participant, "drive spoof");

        let devices = joypad_devices_sample_from(api::joypad::Devices {
            available: vec![api::joypad::Device {
                id: "pad\u{1b}[2J".to_string(),
                name: "Pad\nname".to_string(),
                status: api::joypad::DeviceStatus::Ready,
            }],
            selected: Some("pad\u{1b}[2J".to_string()),
            enabled: false,
            unavailable_reason: Some("reason\rline".to_string()),
            last_error: Some("error\tline".to_string()),
        });
        assert_eq!(devices.available[0].id, "pad");
        assert_eq!(devices.available[0].name, "Pad name");
        assert_eq!(devices.selected.as_deref(), Some("pad"));
        assert_eq!(devices.unavailable_reason.as_deref(), Some("reason line"));
        assert_eq!(devices.last_error.as_deref(), Some("error line"));
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
        assert!(router.topics.iter().any(|metric| metric.aggregate_overflow));
        assert!(router.topics.iter().all(|metric| {
            metric.topic.chars().count() <= MAX_REMOTE_TEXT_CHARS
                && metric.from_participant.chars().count() <= MAX_REMOTE_TEXT_CHARS
        }));

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

        let device = device_sample_from(state_api::tool::device::Record {
            sequence: 1,
            sample: state_api::tool::device::Sample {
                cpu_pct: None,
                ram_used_bytes: None,
                ram_total_bytes: None,
                swap_used_bytes: None,
                swap_total_bytes: None,
                load_1m: None,
                load_5m: None,
                load_15m: None,
                uptime_s: None,
                disks: Some(
                    (0..(MAX_DEVICE_DISKS + 4))
                        .map(|index| state_api::tool::device::Disk {
                            mount_point: format!("/disk-{index}"),
                            file_system: "fs".to_string(),
                            used_bytes: 0,
                            total_bytes: 1,
                        })
                        .collect(),
                ),
                window_ns: 1,
            },
            truncated: 3,
        });
        assert_eq!(
            device.disks.as_deref().map_or(0, Vec::len),
            MAX_DEVICE_DISKS
        );
        assert_eq!(device.disks_truncated, 7);
        assert!(device.cpu_pct.is_none());
    }

    #[test]
    fn local_router_truncation_folds_into_an_existing_overflow_sentinel() {
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

    #[test]
    fn telemetry_snapshots_share_large_latest_value_storage() {
        let telemetry = TelemetryBackend::new();
        telemetry.record_router_at(
            scope("r1"),
            Instant::now(),
            RouterMetricsSample {
                topics: Arc::new(vec![TopicMetric {
                    topic: "v1/motion/state".to_string(),
                    from_participant: "motion".to_string(),
                    ingress_rate_hz: 1.0,
                    count: 1,
                    aggregate_overflow: false,
                }]),
                ..RouterMetricsSample::default()
            },
        );
        let first = telemetry
            .snapshot(&scope("r1"))
            .router
            .expect("first router sample");
        let second = telemetry
            .snapshot(&scope("r1"))
            .router
            .expect("second router sample");
        assert!(Arc::ptr_eq(&first.value.topics, &second.value.topics));
    }

    #[test]
    fn router_snapshot_is_explicitly_scoped_without_cross_robot_flicker() {
        let telemetry = TelemetryBackend::new();
        let now = Instant::now();
        telemetry.record_router_at(
            scope("r1"),
            now,
            RouterMetricsSample {
                throughput_msg_s: 1.0,
                ..RouterMetricsSample::default()
            },
        );
        telemetry.record_router_at(
            scope("r2"),
            now + Duration::from_secs(1),
            RouterMetricsSample {
                throughput_msg_s: 2.0,
                ..RouterMetricsSample::default()
            },
        );

        let r1 = telemetry.snapshot(&scope("r1"));
        let r2 = telemetry.snapshot(&scope("r2"));
        assert_eq!(r1.router.unwrap().value.throughput_msg_s, 1.0);
        assert_eq!(
            r1.router_throughput_history
                .iter()
                .map(|sample| sample.value)
                .collect::<Vec<_>>(),
            vec![1.0]
        );
        assert_eq!(r2.router.unwrap().value.throughput_msg_s, 2.0);
        assert_eq!(
            r2.router_throughput_history
                .iter()
                .map(|sample| sample.value)
                .collect::<Vec<_>>(),
            vec![2.0]
        );
    }

    #[test]
    fn simulation_clock_feed_reaches_the_tui_snapshot() {
        let telemetry = TelemetryBackend::new();
        let (tx, rx) = watch::channel(ClockObservation::default());
        telemetry.set_clock_feed(rx);
        assert!(telemetry.snapshot(&scope("r1")).clock.is_none());
        tx.send_modify(|observation| {
            observation.latest =
                Some(phoxal_cli_core::session::telemetry::ClockSample { now_ns: 5, step: 3 });
            observation.received_at = Some(Instant::now());
        });
        assert_eq!(
            telemetry
                .snapshot(&scope("r1"))
                .clock
                .expect("clock sample")
                .value
                .step,
            3
        );
    }

    #[test]
    fn joypad_command_without_a_running_feed_does_not_panic() {
        let telemetry = TelemetryBackend::new();
        // No feed installed a sender yet - the rejected action is logged and
        // must not panic or block the input path.
        telemetry.send_joypad_command(JoypadCommand::Rescan);
    }
}
