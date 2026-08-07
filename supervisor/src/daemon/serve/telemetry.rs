//! The supervisor's bounded runtime-telemetry collector.
//!
//! Participants publish periodic runtime rollups; the supervisor retains a
//! bounded five-minute history of them and answers
//! `supervisor/telemetry/{snapshot,follow}` from it.
//!
//! Three independent bounds, all of which must hold at once: an age horizon, a
//! record count, and an absolute retained-byte cap. The last one is what makes
//! this safe against a participant that reports maximal rollups at speed -
//! records evicted by it are counted separately, because "you are past five
//! minutes" and "the supervisor ran out of room" are different facts.
//!
//! Per record the topic rows are re-aggregated (a producer may report the same
//! topic/direction/buffer twice) and then bounded, with everything past the row
//! limit folded into one explicit overflow row rather than dropped silently.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use phoxal_api::v0_1 as api;
use phoxal_bus::{
    Bus, Codec, ContractBody, DiagnosticPublisher, MessagePack, QueryFailure, Subscribe,
    Subscriber, Topic,
};
use phoxal_supervisor_api::{
    Cursor, LogText, Name, RuntimeBufferKind, RuntimeDirection, RuntimeStep, RuntimeTopic,
    TelemetryRecord, supervisor,
};

const RETENTION: Duration = Duration::from_secs(5 * 60);
const MAX_RECORDS: usize = 4_096;
/// Sum of retained records' exact encoded sizes. Container bookkeeping is
/// bounded separately by `MAX_RECORDS`.
const MAX_RETAINED_BYTES: usize = 12 * 1024 * 1024;
const MAX_TOPIC_ROWS: usize = 256;
const INGEST_QUEUE_DEPTH: usize = 128;
const DEFAULT_QUERY_RECORDS: usize = 64;
const MAX_QUERY_RECORDS: usize = 64;

#[derive(Debug)]
struct Retained {
    received_at: Instant,
    encoded_bytes: usize,
    record: TelemetryRecord,
}

#[derive(Debug)]
pub(crate) struct TelemetryHistory {
    generation: Name,
    sequence: u64,
    capacity_evictions: u64,
    retained_bytes: usize,
    records: VecDeque<Retained>,
}

impl TelemetryHistory {
    pub(crate) fn new(generation: Name) -> Self {
        Self {
            generation,
            sequence: 0,
            capacity_evictions: 0,
            retained_bytes: 0,
            records: VecDeque::new(),
        }
    }

    pub(crate) fn ingest(
        &mut self,
        now: Instant,
        participant: &str,
        rollup: api::supervisor::telemetry::Rollup,
    ) -> Result<supervisor::telemetry::Follow> {
        self.prune(now);
        let sequence = self
            .sequence
            .checked_add(1)
            .expect("telemetry ingest sequence exhausted");
        let truncated = u32::from(participant.len() > Name::MAX_BYTES);
        let (topics, overflow) = bounded_rows(rollup.topics, rollup.overflow);
        let record = TelemetryRecord {
            sequence,
            participant: Name::new(participant),
            truncated,
            window_ns: rollup.window_ns,
            step: rollup.step.map(step),
            topics,
            overflow,
        };
        let encoded_bytes = MessagePack::encode(&record)?.len();
        self.sequence = sequence;
        self.retained_bytes = self.retained_bytes.saturating_add(encoded_bytes);
        self.records.push_back(Retained {
            received_at: now,
            encoded_bytes,
            record: record.clone(),
        });
        while self.records.len() > MAX_RECORDS || self.retained_bytes > MAX_RETAINED_BYTES {
            self.pop_front(true);
        }
        Ok(supervisor::telemetry::Follow::V0 {
            cursor: self.cursor(),
            record,
        })
    }

    /// One backward page, oldest-first within the page.
    pub(crate) fn page(
        &mut self,
        now: Instant,
        request: &supervisor::telemetry::SnapshotRequest,
    ) -> supervisor::telemetry::Snapshot {
        self.prune(now);
        let supervisor::telemetry::SnapshotRequest::V0 {
            participant,
            limit,
            before_sequence,
        } = request;
        let limit = if *limit == 0 {
            DEFAULT_QUERY_RECORDS
        } else {
            usize::try_from(*limit).unwrap_or(MAX_QUERY_RECORDS)
        }
        .min(MAX_QUERY_RECORDS);
        let matches = |retained: &&Retained| {
            participant
                .as_ref()
                .is_none_or(|wanted| retained.record.participant == *wanted)
        };
        let mut records: Vec<_> = self
            .records
            .iter()
            .rev()
            .filter(|retained| {
                before_sequence.is_none_or(|before| retained.record.sequence < before)
            })
            .filter(matches)
            .take(limit)
            .map(|retained| retained.record.clone())
            .collect();
        records.reverse();
        let next_before_sequence = records.first().and_then(|first| {
            self.records
                .iter()
                .filter(matches)
                .any(|retained| retained.record.sequence < first.sequence)
                .then_some(first.sequence)
        });
        supervisor::telemetry::Snapshot::V0 {
            cursor: self.cursor(),
            records,
            capacity_evictions: self.capacity_evictions,
            next_before_sequence,
        }
    }

    fn prune(&mut self, now: Instant) {
        while self
            .records
            .front()
            .is_some_and(|retained| now.saturating_duration_since(retained.received_at) > RETENTION)
        {
            self.pop_front(false);
        }
    }

    fn pop_front(&mut self, capacity_eviction: bool) {
        let Some(removed) = self.records.pop_front() else {
            return;
        };
        self.retained_bytes = self.retained_bytes.saturating_sub(removed.encoded_bytes);
        if capacity_eviction {
            self.capacity_evictions = self.capacity_evictions.saturating_add(1);
        }
    }

    fn cursor(&self) -> Cursor {
        Cursor {
            generation: self.generation.clone(),
            sequence: self.sequence,
        }
    }
}

const fn step(value: api::supervisor::RuntimeStep) -> RuntimeStep {
    RuntimeStep {
        target_period_ns: value.target_period_ns,
        completed: value.completed,
        errors: value.errors,
        mean_duration_ns: value.mean_duration_ns,
        max_duration_ns: value.max_duration_ns,
        mean_lateness_ns: value.mean_lateness_ns,
        max_lateness_ns: value.max_lateness_ns,
        missed_ticks: value.missed_ticks,
        overruns: value.overruns,
    }
}

const fn direction(value: api::supervisor::RuntimeDirection) -> RuntimeDirection {
    match value {
        api::supervisor::RuntimeDirection::Publish => RuntimeDirection::Publish,
        api::supervisor::RuntimeDirection::Subscribe => RuntimeDirection::Subscribe,
        api::supervisor::RuntimeDirection::Mixed => RuntimeDirection::Mixed,
    }
}

const fn buffer_kind(value: api::supervisor::RuntimeBufferKind) -> RuntimeBufferKind {
    match value {
        api::supervisor::RuntimeBufferKind::Outbound => RuntimeBufferKind::Outbound,
        api::supervisor::RuntimeBufferKind::Latest => RuntimeBufferKind::Latest,
        api::supervisor::RuntimeBufferKind::Subscriber => RuntimeBufferKind::Subscriber,
        api::supervisor::RuntimeBufferKind::Mixed => RuntimeBufferKind::Mixed,
    }
}

fn row(value: api::supervisor::RuntimeTopic) -> RuntimeTopic {
    RuntimeTopic {
        topic: LogText::new(&value.topic),
        direction: direction(value.direction),
        buffer_kind: buffer_kind(value.buffer_kind),
        count: value.count,
        rate_hz: finite_rate(value.rate_hz),
        drops: value.drops,
        latest_overwrites: value.latest_overwrites,
        bounded_evictions: value.bounded_evictions,
        capacity: value.capacity,
        current_depth: value.current_depth,
        high_water_depth: value.high_water_depth,
        decode_errors: value.decode_errors,
        overflowed_rows: value.overflowed_rows,
    }
}

/// Re-aggregate duplicate rows, then bound the row count by folding the tail
/// into one explicit overflow row.
///
/// The producer's own overflow row, when it sent one, is the seed: its omitted
/// count is preserved so a client sees the total that never reached it, not
/// just the part this supervisor folded.
fn bounded_rows(
    topics: Vec<api::supervisor::RuntimeTopic>,
    overflow: Option<api::supervisor::RuntimeTopic>,
) -> (Vec<RuntimeTopic>, Option<RuntimeTopic>) {
    let mut aggregated: BTreeMap<(String, RuntimeDirection, RuntimeBufferKind), RuntimeTopic> =
        BTreeMap::new();
    for value in topics {
        let converted = row(value);
        let key = (
            converted.topic.as_str().to_string(),
            converted.direction,
            converted.buffer_kind,
        );
        match aggregated.get_mut(&key) {
            Some(existing) => merge(existing, &converted),
            None => {
                aggregated.insert(key, converted);
            }
        }
    }

    let mut rows: Vec<_> = aggregated.into_values().collect();
    let mut folded = overflow.map(row);
    if rows.len() > MAX_TOPIC_ROWS {
        let tail = rows.split_off(MAX_TOPIC_ROWS);
        let mut row = folded.take().unwrap_or_else(empty_overflow);
        for omitted in &tail {
            merge(&mut row, omitted);
        }
        row.topic = LogText::new("");
        row.direction = RuntimeDirection::Mixed;
        row.buffer_kind = RuntimeBufferKind::Mixed;
        row.overflowed_rows = row
            .overflowed_rows
            .saturating_add(u32::try_from(tail.len()).unwrap_or(u32::MAX));
        folded = Some(row);
    }
    (rows, folded)
}

fn merge(target: &mut RuntimeTopic, source: &RuntimeTopic) {
    target.count = target.count.saturating_add(source.count);
    target.rate_hz = add_finite_rates(target.rate_hz, source.rate_hz);
    target.drops = target.drops.saturating_add(source.drops);
    target.latest_overwrites = target
        .latest_overwrites
        .saturating_add(source.latest_overwrites);
    target.bounded_evictions = target
        .bounded_evictions
        .saturating_add(source.bounded_evictions);
    target.capacity = target.capacity.saturating_add(source.capacity);
    target.current_depth = target.current_depth.saturating_add(source.current_depth);
    target.high_water_depth = target
        .high_water_depth
        .saturating_add(source.high_water_depth);
    target.decode_errors = target.decode_errors.saturating_add(source.decode_errors);
}

fn finite_rate(rate_hz: f32) -> f32 {
    if rate_hz.is_nan() || rate_hz <= 0.0 {
        0.0
    } else if rate_hz.is_infinite() {
        f32::MAX
    } else {
        rate_hz
    }
}

fn add_finite_rates(left: f32, right: f32) -> f32 {
    let left = finite_rate(left);
    let right = finite_rate(right);
    if left > f32::MAX - right {
        f32::MAX
    } else {
        left + right
    }
}

fn empty_overflow() -> RuntimeTopic {
    RuntimeTopic {
        topic: LogText::new(""),
        direction: RuntimeDirection::Mixed,
        buffer_kind: RuntimeBufferKind::Mixed,
        count: 0,
        rate_hz: 0.0,
        drops: 0,
        latest_overwrites: 0,
        bounded_evictions: 0,
        capacity: 0,
        current_depth: 0,
        high_water_depth: 0,
        decode_errors: 0,
        overflowed_rows: 0,
    }
}

/// Retain participant runtime rollups on `bus` and serve them back until the
/// session ends.
pub(crate) async fn run(bus: &Bus, generation: Name) -> Result<()> {
    let history = Arc::new(Mutex::new(TelemetryHistory::new(generation)));

    let rollups = Subscriber::<api::supervisor::telemetry::Rollup>::new(
        bus,
        &Topic::<Subscribe<api::supervisor::telemetry::Rollup>>::new_static(
            <api::supervisor::telemetry::Rollup as ContractBody>::TOPIC,
        ),
        INGEST_QUEUE_DEPTH,
    )
    .await
    .context("failed to subscribe to participant runtime rollups")?;
    let follow = DiagnosticPublisher::<supervisor::telemetry::Follow>::new(
        bus.clone(),
        &supervisor::topic::owner().telemetry().follow(),
    )
    .context("failed to declare the telemetry follow publisher")?;
    let snapshots =
        super::declare(bus, supervisor::topic::owner().telemetry().snapshot().key()).await?;

    let query_history = Arc::clone(&history);

    let ingest = async {
        loop {
            let received = match rollups.recv().await {
                Ok(received) => received,
                Err(error) => {
                    tracing::debug!("telemetry ingest stopped: {error}");
                    return;
                }
            };
            let participant = received.metadata.participant;
            let item = history
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .ingest(Instant::now(), &participant, received.body);
            match item {
                Ok(item) => {
                    if let Err(error) = follow.publish(item) {
                        tracing::debug!("telemetry follow publish failed: {error}");
                    }
                }
                Err(error) => tracing::debug!("a runtime rollup could not be retained: {error}"),
            }
        }
    };

    let serve = async {
        loop {
            let incoming = match snapshots.recv().await {
                Ok(incoming) => incoming,
                Err(error) => {
                    tracing::debug!("telemetry snapshot server stopped: {error}");
                    return;
                }
            };
            let request = match MessagePack::decode::<supervisor::telemetry::SnapshotRequest>(
                &incoming.request_bytes().unwrap_or_default(),
            ) {
                Ok(request) => request,
                Err(error) => {
                    let _ = incoming
                        .reply_err(&QueryFailure::invalid_argument(format!(
                            "malformed telemetry/snapshot request: {error}"
                        )))
                        .await;
                    continue;
                }
            };
            let page = query_history
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .page(Instant::now(), &request);
            match MessagePack::encode(&page) {
                Ok(payload) => {
                    if let Err(error) = incoming.reply(bus, payload).await {
                        tracing::debug!("telemetry snapshot reply failed: {error}");
                    }
                }
                Err(error) => {
                    let _ = incoming
                        .reply_err(&QueryFailure::internal(format!(
                            "failed to encode a telemetry page: {error}"
                        )))
                        .await;
                }
            }
        }
    };

    tracing::debug!("the supervisor is retaining participant runtime rollups");
    tokio::join!(ingest, serve);
    Ok(())
}

/// A fresh opaque generation for this collector's cursor.
pub(crate) fn generation() -> Result<Name> {
    super::logs::generation()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rollup(topics: Vec<api::supervisor::RuntimeTopic>) -> api::supervisor::telemetry::Rollup {
        api::supervisor::telemetry::Rollup {
            window_ns: 1_000_000_000,
            step: None,
            topics,
            overflow: None,
        }
    }

    fn topic(name: &str) -> api::supervisor::RuntimeTopic {
        api::supervisor::RuntimeTopic {
            topic: name.to_string(),
            direction: api::supervisor::RuntimeDirection::Publish,
            buffer_kind: api::supervisor::RuntimeBufferKind::Outbound,
            count: 1,
            rate_hz: 1.0,
            drops: 0,
            latest_overwrites: 0,
            bounded_evictions: 0,
            capacity: 64,
            current_depth: 0,
            high_water_depth: 0,
            decode_errors: 0,
            timeline_filtered: 0,
            overflowed_rows: 0,
        }
    }

    fn history() -> TelemetryHistory {
        TelemetryHistory::new(Name::new("generation-a"))
    }

    fn request(
        participant: Option<&str>,
        limit: u32,
        before_sequence: Option<u64>,
    ) -> supervisor::telemetry::SnapshotRequest {
        supervisor::telemetry::SnapshotRequest::V0 {
            participant: participant.map(Name::new),
            limit,
            before_sequence,
        }
    }

    #[test]
    fn duplicate_rows_are_re_aggregated_rather_than_repeated() {
        let (rows, overflow) = bounded_rows(vec![topic("drive/state"), topic("drive/state")], None);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].count, 2);
        assert!((rows[0].rate_hz - 2.0).abs() < f32::EPSILON);
        assert!(overflow.is_none());
    }

    #[test]
    fn rows_past_the_bound_are_folded_into_one_explicit_overflow_row() {
        let topics = (0..MAX_TOPIC_ROWS + 10)
            .map(|index| topic(&format!("topic/{index:04}")))
            .collect();
        let (rows, overflow) = bounded_rows(topics, None);
        assert_eq!(rows.len(), MAX_TOPIC_ROWS);
        let overflow = overflow.expect("the tail is reported, never dropped");
        assert_eq!(overflow.overflowed_rows, 10);
        assert_eq!(overflow.count, 10);
        assert!(overflow.topic.is_empty());
        assert_eq!(overflow.direction, RuntimeDirection::Mixed);
        assert_eq!(overflow.buffer_kind, RuntimeBufferKind::Mixed);
    }

    #[test]
    fn a_malformed_rate_is_clamped_before_it_can_be_retained() {
        let mut hostile = topic("drive/state");
        hostile.rate_hz = f32::NAN;
        let (rows, _) = bounded_rows(vec![hostile], None);
        assert_eq!(rows[0].rate_hz, 0.0);

        let mut infinite = topic("drive/state");
        infinite.rate_hz = f32::INFINITY;
        let (rows, _) = bounded_rows(vec![infinite], None);
        assert!(rows[0].rate_hz.is_finite());
    }

    #[test]
    fn the_age_horizon_and_the_memory_cap_are_counted_apart() {
        let mut history = history();
        let start = Instant::now();
        history
            .ingest(start, "drive", rollup(vec![topic("a")]))
            .expect("ingest");
        // Aged out: not a capacity eviction, because the supervisor did not run
        // out of room - the record simply got old.
        let supervisor::telemetry::Snapshot::V0 {
            records,
            capacity_evictions,
            ..
        } = history.page(
            start + RETENTION + Duration::from_secs(1),
            &request(None, 0, None),
        );
        assert!(records.is_empty());
        assert_eq!(capacity_evictions, 0);

        let mut history = TelemetryHistory::new(Name::new("generation-b"));
        for _ in 0..MAX_RECORDS + 5 {
            history
                .ingest(start, "drive", rollup(vec![topic("a")]))
                .expect("ingest");
        }
        let supervisor::telemetry::Snapshot::V0 {
            capacity_evictions, ..
        } = history.page(start, &request(None, 0, None));
        assert_eq!(capacity_evictions, 5);
    }

    #[test]
    fn a_page_walks_backwards_within_one_participants_history() {
        let mut history = history();
        let now = Instant::now();
        for index in 0..6 {
            let participant = if index % 2 == 0 { "drive" } else { "brain" };
            history
                .ingest(now, participant, rollup(vec![topic("a")]))
                .expect("ingest");
        }
        let supervisor::telemetry::Snapshot::V0 {
            records,
            next_before_sequence,
            ..
        } = history.page(now, &request(Some("drive"), 2, None));
        assert_eq!(
            records
                .iter()
                .map(|record| record.sequence)
                .collect::<Vec<_>>(),
            [3, 5]
        );
        assert!(
            records
                .iter()
                .all(|record| record.participant.as_str() == "drive")
        );

        let supervisor::telemetry::Snapshot::V0 {
            records,
            next_before_sequence,
            ..
        } = history.page(now, &request(Some("drive"), 2, next_before_sequence));
        assert_eq!(
            records
                .iter()
                .map(|record| record.sequence)
                .collect::<Vec<_>>(),
            [1]
        );
        assert_eq!(next_before_sequence, None);
    }

    #[test]
    fn an_oversized_participant_marks_the_record_truncated() {
        let mut history = history();
        let supervisor::telemetry::Follow::V0 { record, .. } = history
            .ingest(
                Instant::now(),
                &"p".repeat(Name::MAX_BYTES + 1),
                rollup(Vec::new()),
            )
            .expect("ingest");
        assert_eq!(record.truncated, 1);
        assert!(record.participant.as_str().len() <= Name::MAX_BYTES);
    }
}
