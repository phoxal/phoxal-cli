//! The supervisor's bounded runtime-telemetry collector.
//!
//! `tool/telemetry` used to retain participant runtime rollups and serve them
//! back (organization#978). The supervisor already supervises every
//! participant, so it retains their rollups itself and answers
//! `supervisor/telemetry/{snapshot,follow}` from its own bounded history.
//!
//! The retention is a faithful move, not a redesign: the same five-minute
//! window, the same 4,096-record and 12 MiB ceilings, the same topic-row
//! aggregation and overflow accounting. The bounds keep a complete snapshot
//! inside the bus body ceiling even when every producer reports at its maximum.

use anyhow::{Context, Result};
use phoxal_api::v0_1 as api;
use phoxal_bus::{
    Bus, Codec, ContractBody, DiagnosticPublisher, MessagePack, QueryFailure, Subscribe,
    Subscriber, Topic,
};
use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const RUNTIME_RETENTION: Duration = Duration::from_secs(5 * 60);
const MAX_RUNTIME_RECORDS: usize = 4_096;
/// Sum of retained records' exact MessagePack sizes. Container bookkeeping is
/// fixed/bounded separately by `MAX_RUNTIME_RECORDS`.
const MAX_RUNTIME_RETAINED_BYTES: usize = 12 * 1024 * 1024;
const MAX_RUNTIME_TOPIC_ROWS: usize = 256;
const MAX_RUNTIME_PARTICIPANT_BYTES: usize = 512;
const MAX_RUNTIME_TOPIC_BYTES: usize = 256;
const DEFAULT_RUNTIME_QUERY_RECORDS: usize = 64;
const MAX_RUNTIME_QUERY_RECORDS: usize = 64;
/// The bus body ceiling a complete snapshot must stay inside.
#[cfg(test)]
const MAX_RUNTIME_SNAPSHOT_WIRE_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug)]
struct TimedRuntimeRecord {
    received_at: Instant,
    encoded_bytes: usize,
    record: api::supervisor::telemetry::Record,
}

#[derive(Debug)]
struct RuntimeHistory {
    generation: String,
    sequence: u64,
    capacity_evictions: u64,
    /// Exact MessagePack byte sum for `records`, maintained on every push/pop.
    retained_bytes: usize,
    records: VecDeque<TimedRuntimeRecord>,
}

impl RuntimeHistory {
    fn new(generation: String) -> Self {
        Self {
            generation,
            sequence: 0,
            capacity_evictions: 0,
            retained_bytes: 0,
            records: VecDeque::new(),
        }
    }

    fn ingest(
        &mut self,
        now: Instant,
        participant_id: String,
        rollup: api::supervisor::telemetry::Rollup,
    ) -> Result<api::supervisor::telemetry::Follow> {
        self.prune(now);
        let sequence = self
            .sequence
            .checked_add(1)
            .expect("telemetry sequence exhausted");
        let participant_was_truncated = participant_id.len() > MAX_RUNTIME_PARTICIPANT_BYTES;
        let participant_id = truncate_utf8(participant_id, MAX_RUNTIME_PARTICIPANT_BYTES);
        let (topics, overflow) = bounded_runtime_topics(rollup.topics, rollup.overflow);
        let record = api::supervisor::telemetry::Record {
            sequence,
            participant_id,
            truncated: u32::from(participant_was_truncated),
            window_ns: rollup.window_ns,
            step: rollup.step,
            topics,
            overflow,
        };
        let encoded_bytes = MessagePack::encode(&record)?.len();
        self.sequence = sequence;
        self.retained_bytes = self.retained_bytes.saturating_add(encoded_bytes);
        self.records.push_back(TimedRuntimeRecord {
            received_at: now,
            encoded_bytes,
            record: record.clone(),
        });
        while self.records.len() > MAX_RUNTIME_RECORDS
            || self.retained_bytes > MAX_RUNTIME_RETAINED_BYTES
        {
            self.pop_front(true);
        }
        Ok(api::supervisor::telemetry::Follow {
            cursor: self.cursor(),
            record,
        })
    }

    fn snapshot(
        &mut self,
        now: Instant,
        request: &api::supervisor::telemetry::SnapshotRequest,
    ) -> api::supervisor::telemetry::Snapshot {
        self.prune(now);
        let requested = if request.limit == 0 {
            DEFAULT_RUNTIME_QUERY_RECORDS
        } else {
            usize::try_from(request.limit).unwrap_or(usize::MAX)
        };
        let limit = requested.min(MAX_RUNTIME_QUERY_RECORDS);
        let mut records = self
            .records
            .iter()
            .rev()
            .filter(|timed| {
                request
                    .before_sequence
                    .is_none_or(|before| timed.record.sequence < before)
                    && request
                        .participant_id
                        .as_ref()
                        .is_none_or(|participant| timed.record.participant_id == *participant)
            })
            .take(limit)
            .map(|timed| timed.record.clone())
            .collect::<Vec<_>>();
        records.reverse();
        let next_before_sequence = records.first().and_then(|first| {
            self.records
                .iter()
                .any(|timed| {
                    timed.record.sequence < first.sequence
                        && request
                            .participant_id
                            .as_ref()
                            .is_none_or(|participant| timed.record.participant_id == *participant)
                })
                .then_some(first.sequence)
        });
        api::supervisor::telemetry::Snapshot {
            cursor: self.cursor(),
            records,
            capacity_evictions: self.capacity_evictions,
            next_before_sequence,
        }
    }

    fn prune(&mut self, now: Instant) {
        while self.records.front().is_some_and(|record| {
            now.saturating_duration_since(record.received_at) > RUNTIME_RETENTION
        }) {
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

    fn cursor(&self) -> api::supervisor::Cursor {
        api::supervisor::Cursor {
            generation: self.generation.clone(),
            sequence: self.sequence,
        }
    }
}

fn bounded_runtime_topics(
    topics: Vec<api::supervisor::RuntimeTopic>,
    overflow: Option<api::supervisor::RuntimeTopic>,
) -> (
    Vec<api::supervisor::RuntimeTopic>,
    Option<api::supervisor::RuntimeTopic>,
) {
    let mut overflow_rows = overflow
        .map(normalize_runtime_overflow)
        .map(|row| {
            let omitted_rows = row.overflowed_rows;
            (row, omitted_rows)
        })
        .into_iter()
        .collect::<Vec<_>>();
    let mut normal = BTreeMap::<
        (
            String,
            api::supervisor::RuntimeDirection,
            api::supervisor::RuntimeBufferKind,
        ),
        Vec<api::supervisor::RuntimeTopic>,
    >::new();
    for mut row in topics {
        row.rate_hz = finite_rate(row.rate_hz);
        let is_normal = !row.topic.is_empty()
            && row.topic.len() <= MAX_RUNTIME_TOPIC_BYTES
            && row.direction != api::supervisor::RuntimeDirection::Mixed
            && row.buffer_kind != api::supervisor::RuntimeBufferKind::Mixed
            && row.overflowed_rows == 0;
        if is_normal {
            let key = (row.topic.clone(), row.direction, row.buffer_kind);
            normal.entry(key).or_default().push(row);
        } else {
            let omitted_rows = row.overflowed_rows.saturating_add(1);
            overflow_rows.push((row, omitted_rows));
        }
    }

    let mut retained = Vec::with_capacity(normal.len().min(MAX_RUNTIME_TOPIC_ROWS));
    for rows in normal.into_values() {
        let row = aggregate_runtime_values(rows);
        if retained.len() < MAX_RUNTIME_TOPIC_ROWS {
            retained.push(row);
        } else {
            overflow_rows.push((row, 1));
        }
    }
    (retained, aggregate_runtime_overflow(overflow_rows))
}

fn normalize_runtime_overflow(
    mut row: api::supervisor::RuntimeTopic,
) -> api::supervisor::RuntimeTopic {
    row.topic.clear();
    row.direction = api::supervisor::RuntimeDirection::Mixed;
    row.buffer_kind = api::supervisor::RuntimeBufferKind::Mixed;
    row.rate_hz = finite_rate(row.rate_hz);
    row
}

fn aggregate_runtime_values(
    mut rows: Vec<api::supervisor::RuntimeTopic>,
) -> api::supervisor::RuntimeTopic {
    // Floating-point addition is not associative, so every identity uses one
    // canonical rate order regardless of producer row order.
    rows.sort_by(|left, right| left.rate_hz.total_cmp(&right.rate_hz));
    let mut rows = rows.into_iter();
    let mut target = rows.next().expect("runtime value group is never empty");
    for row in rows {
        merge_runtime_values(&mut target, &row);
    }
    target
}

fn aggregate_runtime_overflow(
    mut rows: Vec<(api::supervisor::RuntimeTopic, u32)>,
) -> Option<api::supervisor::RuntimeTopic> {
    // Invalid, excess, and producer-overflow rows share the same canonical
    // rate fold as retained duplicate identities.
    rows.sort_by(|(left, _), (right, _)| left.rate_hz.total_cmp(&right.rate_hz));
    if rows.is_empty() {
        return None;
    }

    let mut target = empty_runtime_overflow();
    for (row, omitted_rows) in rows {
        merge_runtime_values(&mut target, &row);
        target.overflowed_rows = target.overflowed_rows.saturating_add(omitted_rows);
    }
    Some(target)
}

fn merge_runtime_values(
    target: &mut api::supervisor::RuntimeTopic,
    row: &api::supervisor::RuntimeTopic,
) {
    target.count = target.count.saturating_add(row.count);
    target.rate_hz = add_finite_rates(target.rate_hz, row.rate_hz);
    target.drops = target.drops.saturating_add(row.drops);
    target.latest_overwrites = target
        .latest_overwrites
        .saturating_add(row.latest_overwrites);
    target.bounded_evictions = target
        .bounded_evictions
        .saturating_add(row.bounded_evictions);
    target.capacity = target.capacity.saturating_add(row.capacity);
    target.current_depth = target.current_depth.saturating_add(row.current_depth);
    target.high_water_depth = target.high_water_depth.saturating_add(row.high_water_depth);
    target.decode_errors = target.decode_errors.saturating_add(row.decode_errors);
    target.timeline_filtered = target
        .timeline_filtered
        .saturating_add(row.timeline_filtered);
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

fn empty_runtime_overflow() -> api::supervisor::RuntimeTopic {
    api::supervisor::RuntimeTopic {
        topic: String::new(),
        direction: api::supervisor::RuntimeDirection::Mixed,
        buffer_kind: api::supervisor::RuntimeBufferKind::Mixed,
        count: 0,
        rate_hz: 0.0,
        drops: 0,
        latest_overwrites: 0,
        bounded_evictions: 0,
        capacity: 0,
        current_depth: 0,
        high_water_depth: 0,
        decode_errors: 0,
        timeline_filtered: 0,
        overflowed_rows: 0,
    }
}

/// Truncate on a char boundary, never mid-codepoint.
fn truncate_utf8(mut value: String, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value;
    }
    let mut boundary = max_bytes;
    while !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    value.truncate(boundary);
    value
}

/// Retain participant runtime rollups on `bus` and serve them back until the
/// session ends.
///
/// Two concurrent halves, as the deleted tool had them: an ingest loop draining
/// the rollup subscription into the bounded history and republishing each
/// retained record on `follow`, and a query loop answering `snapshot` from that
/// same history.
pub(crate) async fn run(bus: &Bus, generation: String) -> Result<()> {
    let history = Arc::new(Mutex::new(RuntimeHistory::new(generation)));

    let rollups = Subscriber::<api::supervisor::telemetry::Rollup>::new(
        bus,
        &Topic::<Subscribe<api::supervisor::telemetry::Rollup>>::new_static(
            <api::supervisor::telemetry::Rollup as ContractBody>::TOPIC,
        ),
        128,
    )
    .await
    .context("failed to subscribe to runtime rollups")?;
    let follow = DiagnosticPublisher::<api::supervisor::telemetry::Follow>::new(
        bus.clone(),
        &Topic::new_static(<api::supervisor::telemetry::Follow as ContractBody>::TOPIC),
    )
    .context("failed to declare the telemetry follow publisher")?;
    let snapshots = bus
        .declare_server(<api::supervisor::telemetry::SnapshotRequest as ContractBody>::TOPIC)
        .await
        .context("failed to declare the telemetry snapshot server")?;

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
            let participant_id = received.metadata.participant;
            let item = {
                let mut history = history
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                history.ingest(Instant::now(), participant_id, received.body)
            };
            match item {
                Ok(item) => {
                    if let Err(error) = follow.publish(item) {
                        tracing::debug!("telemetry follow publish failed: {error}");
                    }
                }
                Err(error) => {
                    tracing::debug!("runtime rollup could not be retained: {error}");
                }
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
            let request = match MessagePack::decode::<api::supervisor::telemetry::SnapshotRequest>(
                &incoming.request_bytes().unwrap_or_default(),
            ) {
                Ok(request) => request,
                Err(error) => {
                    let _ = incoming
                        .reply_err(&QueryFailure::invalid_argument(format!(
                            "decode telemetry snapshot request: {error}"
                        )))
                        .await;
                    continue;
                }
            };
            let snapshot = {
                let mut history = query_history
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                history.snapshot(Instant::now(), &request)
            };
            match MessagePack::encode(&snapshot) {
                Ok(payload) => {
                    if let Err(error) = incoming.reply(bus, payload).await {
                        tracing::debug!("telemetry snapshot reply failed: {error}");
                    }
                }
                Err(error) => {
                    let _ = incoming
                        .reply_err(&QueryFailure::internal(format!(
                            "encode telemetry snapshot: {error}"
                        )))
                        .await;
                }
            }
        }
    };

    tracing::debug!("supervisor is retaining runtime telemetry");
    tokio::join!(ingest, serve);
    Ok(())
}

/// A fresh opaque generation for this collector's cursor.
pub(crate) fn generation() -> Result<String> {
    let mut random = [0_u8; 16];
    getrandom::fill(&mut random).map_err(|error| {
        anyhow::anyhow!("OS entropy unavailable for telemetry generation: {error}")
    })?;
    Ok(random.iter().map(|byte| format!("{byte:02x}")).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generation_is_opaque_and_unique_per_call() {
        let first = generation().unwrap();
        let second = generation().unwrap();
        assert_eq!(first.len(), 32);
        assert!(first.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert_ne!(first, second);
    }

    fn runtime_rollup(window_ns: u64) -> api::supervisor::telemetry::Rollup {
        api::supervisor::telemetry::Rollup {
            window_ns,
            step: None,
            topics: Vec::new(),
            overflow: None,
        }
    }

    fn runtime_topic(topic: String) -> api::supervisor::RuntimeTopic {
        api::supervisor::RuntimeTopic {
            topic,
            direction: api::supervisor::RuntimeDirection::Publish,
            buffer_kind: api::supervisor::RuntimeBufferKind::Outbound,
            count: 1,
            rate_hz: 1.0,
            drops: 1,
            latest_overwrites: 0,
            bounded_evictions: 0,
            capacity: 1_024,
            current_depth: 1,
            high_water_depth: 1,
            decode_errors: 1,
            timeline_filtered: 1,
            overflowed_rows: 0,
        }
    }

    fn maximal_runtime_rollup() -> api::supervisor::telemetry::Rollup {
        let topics = (0..MAX_RUNTIME_TOPIC_ROWS)
            .map(|index| {
                let prefix = format!("v0.1/{index:03}/");
                runtime_topic(format!(
                    "{prefix}{}",
                    "x".repeat(MAX_RUNTIME_TOPIC_BYTES - prefix.len())
                ))
            })
            .collect();
        api::supervisor::telemetry::Rollup {
            window_ns: u64::MAX,
            step: Some(api::supervisor::RuntimeStep {
                target_period_ns: u64::MAX,
                completed: u64::MAX,
                errors: u64::MAX,
                mean_duration_ns: u64::MAX,
                max_duration_ns: u64::MAX,
                mean_lateness_ns: u64::MAX,
                max_lateness_ns: u64::MAX,
                missed_ticks: u64::MAX,
                overruns: u64::MAX,
            }),
            topics,
            overflow: Some(api::supervisor::RuntimeTopic {
                topic: String::new(),
                direction: api::supervisor::RuntimeDirection::Mixed,
                buffer_kind: api::supervisor::RuntimeBufferKind::Mixed,
                count: u64::MAX,
                rate_hz: f32::MAX,
                drops: u64::MAX,
                latest_overwrites: u64::MAX,
                bounded_evictions: u64::MAX,
                capacity: u64::MAX,
                current_depth: u64::MAX,
                high_water_depth: u64::MAX,
                decode_errors: u64::MAX,
                timeline_filtered: u64::MAX,
                overflowed_rows: u32::MAX,
            }),
        }
    }

    #[test]
    fn runtime_history_retains_five_minutes_and_uses_cursor_sequence() {
        let mut history = RuntimeHistory::new("generation-a".to_string());
        let start = Instant::now();
        let first = history
            .ingest(start, "drive".to_string(), runtime_rollup(1_000_000_000))
            .unwrap();
        let second = history
            .ingest(
                start + Duration::from_secs(1),
                "map".to_string(),
                runtime_rollup(1_000_000_001),
            )
            .unwrap();
        assert_eq!(first.cursor.sequence, 1);
        assert_eq!(second.cursor.sequence, 2);

        let snapshot = history.snapshot(
            start + RUNTIME_RETENTION,
            &api::supervisor::telemetry::SnapshotRequest {
                participant_id: None,
                limit: 0,
                before_sequence: None,
            },
        );
        assert_eq!(snapshot.records.len(), 2);

        let expired = history.snapshot(
            start + RUNTIME_RETENTION + Duration::from_nanos(1),
            &api::supervisor::telemetry::SnapshotRequest {
                participant_id: None,
                limit: 0,
                before_sequence: None,
            },
        );
        assert_eq!(expired.records.len(), 1);
        assert_eq!(expired.records[0].participant_id, "map");
        assert_eq!(expired.cursor.sequence, 2);
    }

    #[test]
    fn runtime_snapshot_filters_and_returns_newest_bounded_records_in_order() {
        let mut history = RuntimeHistory::new("generation-a".to_string());
        let start = Instant::now();
        for index in 0..5 {
            let participant = if index % 2 == 0 { "drive" } else { "map" };
            history
                .ingest(
                    start + Duration::from_secs(index),
                    participant.to_string(),
                    runtime_rollup(index),
                )
                .unwrap();
        }
        let snapshot = history.snapshot(
            start + Duration::from_secs(5),
            &api::supervisor::telemetry::SnapshotRequest {
                participant_id: Some("drive".to_string()),
                limit: 2,
                before_sequence: None,
            },
        );
        assert_eq!(snapshot.cursor.sequence, 5);
        assert_eq!(snapshot.records.len(), 2);
        assert_eq!(snapshot.records[0].window_ns, 2);
        assert_eq!(snapshot.records[1].window_ns, 4);
        assert_eq!(snapshot.next_before_sequence, Some(3));

        let older = history.snapshot(
            start + Duration::from_secs(5),
            &api::supervisor::telemetry::SnapshotRequest {
                participant_id: Some("drive".to_string()),
                limit: 2,
                before_sequence: snapshot.next_before_sequence,
            },
        );
        assert_eq!(older.records.len(), 1);
        assert_eq!(older.records[0].window_ns, 0);
        assert_eq!(older.next_before_sequence, None);
    }

    #[test]
    fn runtime_ingest_clamps_identity_and_rows_with_deterministic_overflow() {
        let mut topics = (0..260)
            .rev()
            .map(|index| runtime_topic(format!("v0.1/test/{index:03}")))
            .collect::<Vec<_>>();
        topics.push(runtime_topic("z".repeat(MAX_RUNTIME_TOPIC_BYTES + 1)));
        let mut source_overflow = empty_runtime_overflow();
        source_overflow.count = 3;
        source_overflow.overflowed_rows = 2;
        let mut history = RuntimeHistory::new("generation-a".to_string());
        let follow = history
            .ingest(
                Instant::now(),
                "participant-é".repeat(MAX_RUNTIME_PARTICIPANT_BYTES),
                api::supervisor::telemetry::Rollup {
                    window_ns: 1,
                    step: None,
                    topics,
                    overflow: Some(source_overflow),
                },
            )
            .unwrap();

        let record = follow.record;
        assert_eq!(record.truncated, 1);
        assert!(record.participant_id.len() <= MAX_RUNTIME_PARTICIPANT_BYTES);
        assert!(
            record
                .participant_id
                .is_char_boundary(record.participant_id.len())
        );
        assert_eq!(record.topics.len(), MAX_RUNTIME_TOPIC_ROWS);
        assert_eq!(record.topics.first().unwrap().topic, "v0.1/test/000");
        assert_eq!(record.topics.last().unwrap().topic, "v0.1/test/255");
        assert!(
            record
                .topics
                .iter()
                .all(|row| row.topic.len() <= MAX_RUNTIME_TOPIC_BYTES)
        );
        let overflow = record.overflow.unwrap();
        assert!(overflow.topic.is_empty());
        // Two source-overflow rows + four excess valid rows + one oversized row.
        assert_eq!(overflow.overflowed_rows, 7);
        assert_eq!(overflow.count, 8);
    }

    #[test]
    fn runtime_ingest_sanitizes_all_non_finite_and_saturating_rates() {
        let mut normal = [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, f32::MAX]
            .into_iter()
            .enumerate()
            .map(|(index, rate_hz)| {
                let mut row = runtime_topic(format!("v0.1/rate/{index}"));
                row.rate_hz = rate_hz;
                row
            })
            .collect::<Vec<_>>();
        for rate_hz in [f32::INFINITY, f32::NEG_INFINITY, f32::MAX, f32::MAX] {
            let mut row = runtime_topic("x".repeat(MAX_RUNTIME_TOPIC_BYTES + 1));
            row.rate_hz = rate_hz;
            normal.push(row);
        }
        let mut source_overflow = empty_runtime_overflow();
        source_overflow.rate_hz = f32::NAN;

        let (topics, overflow) = bounded_runtime_topics(normal, Some(source_overflow));
        let rates = topics.iter().map(|row| row.rate_hz).collect::<Vec<_>>();
        assert_eq!(rates, vec![0.0, f32::MAX, 0.0, f32::MAX]);
        assert!(rates.iter().all(|rate| rate.is_finite()));
        let overflow = overflow.unwrap();
        assert_eq!(overflow.rate_hz, f32::MAX);
        assert!(overflow.rate_hz.is_finite());
    }

    #[test]
    fn shuffled_duplicate_keys_aggregate_before_row_cap_without_double_overflow() {
        const ADVERSARIAL_RATES: [f32; 3] = [323.832_76, 150.849_17, 650.934_45];

        let mut rows = (0..MAX_RUNTIME_TOPIC_ROWS)
            .map(|index| runtime_topic(format!("v0.1/duplicate/{index:03}")))
            .collect::<Vec<_>>();
        let first = rows.first_mut().unwrap();
        first.count = u64::MAX;
        first.rate_hz = ADVERSARIAL_RATES[0];
        first.drops = u64::MAX;
        first.latest_overwrites = u64::MAX;
        first.bounded_evictions = u64::MAX;
        first.capacity = u64::MAX;
        first.current_depth = u64::MAX;
        first.high_water_depth = u64::MAX;
        first.decode_errors = u64::MAX;
        first.timeline_filtered = u64::MAX;

        let mut duplicate = runtime_topic("v0.1/duplicate/000".to_string());
        duplicate.rate_hz = ADVERSARIAL_RATES[1];
        duplicate.latest_overwrites = 1;
        duplicate.bounded_evictions = 1;
        rows.push(duplicate);
        let mut duplicate = runtime_topic("v0.1/duplicate/000".to_string());
        duplicate.rate_hz = ADVERSARIAL_RATES[2];
        rows.push(duplicate);

        for rate_hz in ADVERSARIAL_RATES {
            let mut invalid = runtime_topic("x".repeat(MAX_RUNTIME_TOPIC_BYTES + 1));
            invalid.rate_hz = rate_hz;
            rows.push(invalid);
        }

        let mut source_overflow = empty_runtime_overflow();
        source_overflow.count = 9;
        source_overflow.rate_hz = 3.0;
        source_overflow.overflowed_rows = 2;

        let ordered = bounded_runtime_topics(rows.clone(), Some(source_overflow.clone()));
        rows.rotate_left(73);
        rows.reverse();
        let shuffled = bounded_runtime_topics(rows, Some(source_overflow));
        assert_eq!(
            MessagePack::encode(&ordered).unwrap(),
            MessagePack::encode(&shuffled).unwrap()
        );

        let (topics, overflow) = ordered;
        assert_eq!(topics.len(), MAX_RUNTIME_TOPIC_ROWS);
        let aggregate = topics.first().unwrap();
        assert_eq!(aggregate.topic, "v0.1/duplicate/000");
        assert_eq!(aggregate.count, u64::MAX);
        assert_eq!(aggregate.rate_hz.to_bits(), 0x448c_b3ba);
        assert_eq!(aggregate.drops, u64::MAX);
        assert_eq!(aggregate.latest_overwrites, u64::MAX);
        assert_eq!(aggregate.bounded_evictions, u64::MAX);
        assert_eq!(aggregate.capacity, u64::MAX);
        assert_eq!(aggregate.current_depth, u64::MAX);
        assert_eq!(aggregate.high_water_depth, u64::MAX);
        assert_eq!(aggregate.decode_errors, u64::MAX);
        assert_eq!(aggregate.timeline_filtered, u64::MAX);

        let overflow = overflow.unwrap();
        assert_eq!(overflow.count, 12);
        assert_eq!(overflow.rate_hz.to_bits(), 0x448d_13ba);
        assert_eq!(overflow.overflowed_rows, 5);
    }

    #[test]
    fn runtime_history_is_byte_bounded_and_maximal_query_is_wire_safe() {
        let start = Instant::now();
        let mut history = RuntimeHistory::new("generation-a".to_string());
        for index in 0..MAX_RUNTIME_QUERY_RECORDS {
            history
                .ingest(
                    start + Duration::from_millis(index as u64),
                    "p".repeat(MAX_RUNTIME_PARTICIPANT_BYTES),
                    maximal_runtime_rollup(),
                )
                .unwrap();
        }
        assert_eq!(history.records.len(), MAX_RUNTIME_QUERY_RECORDS);
        assert_eq!(history.capacity_evictions, 0);
        assert!(history.retained_bytes <= MAX_RUNTIME_RETAINED_BYTES);

        let snapshot = history.snapshot(
            start + Duration::from_secs(1),
            &api::supervisor::telemetry::SnapshotRequest {
                participant_id: None,
                limit: MAX_RUNTIME_QUERY_RECORDS as u32,
                before_sequence: None,
            },
        );
        assert_eq!(snapshot.records.len(), MAX_RUNTIME_QUERY_RECORDS);
        let encoded = MessagePack::encode(&snapshot).unwrap();
        assert!(
            encoded.len() <= MAX_RUNTIME_SNAPSHOT_WIRE_BYTES,
            "{}-byte maximal runtime snapshot exceeds the decoder ceiling",
            encoded.len()
        );

        for index in MAX_RUNTIME_QUERY_RECORDS..(MAX_RUNTIME_QUERY_RECORDS * 3) {
            history
                .ingest(
                    start + Duration::from_millis(index as u64),
                    "p".repeat(MAX_RUNTIME_PARTICIPANT_BYTES),
                    maximal_runtime_rollup(),
                )
                .unwrap();
        }
        assert!(history.capacity_evictions > 0);
        assert!(history.retained_bytes <= MAX_RUNTIME_RETAINED_BYTES);
    }
}
