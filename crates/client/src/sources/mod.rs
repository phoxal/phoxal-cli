//! Disposable graph sources and their stateless ingress adapter.

pub(crate) mod input;
pub(crate) mod liveliness;
pub(crate) mod logs;
pub(crate) mod motion;
pub(crate) mod runtimes;

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::convert::Infallible;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use anyhow::{Result, anyhow};
use phoxal_api::v0_1 as api;
use phoxal_api::v0_1 as state_api;
use phoxal_bus::Bus;
use phoxal_bus::{
    CommandPublisher, ContractBody, DEFAULT_QUERY_TIMEOUT, Publish, Querier, Subscribe, Subscriber,
    Topic,
};
use tokio::sync::{Notify, mpsc};

use crate::reconcile::{Cursor, ReconcileOutcome, Reconciler, RetryBackoff, Sequenced};
use phoxal_cli_observation::{
    JoypadDevice, JoypadDeviceStatus, JoypadDevicesSample, RuntimeBufferKind, RuntimeDirection,
    RuntimeFeedStatus, RuntimePerformanceSample, RuntimeStepSample, RuntimeTopicSample,
    sanitize_terminal_text,
};
use phoxal_cli_observation::{RobotScope, SourceStatus};

const MAX_JOYPAD_DEVICES: usize = 64;

const MAX_REMOTE_TEXT_CHARS: usize = 256;

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
}

fn telemetry_slot(update: &TelemetryUpdate) -> Option<String> {
    match update {
        TelemetryUpdate::Joypads(_) => Some("joypads".to_string()),
        TelemetryUpdate::Motion(_) => Some("motion".to_string()),
        TelemetryUpdate::Health(source, _) => Some(format!("health:{source}")),
        TelemetryUpdate::Runtimes(_, _, _) | TelemetryUpdate::Runtime(_, _) => None,
    }
}

impl TelemetryBackend {
    pub(crate) fn with_updates(updates: Arc<TelemetryMailbox>) -> Self {
        Self { updates }
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
}
