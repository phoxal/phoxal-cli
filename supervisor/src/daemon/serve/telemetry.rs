//! Bounded participant runtime-telemetry retention.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use phoxal_bus::{BusHandle, Codec, MessagePack, StreamEvent, StreamPublisher, StreamReceiver};
use phoxal_supervisor_api::{payload, supervisor};
use tokio::task::JoinSet;

const RETENTION: Duration = Duration::from_secs(5 * 60);
const MAX_RECORDS: usize = 4_096;
const MAX_RETAINED_BYTES: usize = 12 * 1024 * 1024;
const MAX_TOPIC_ROWS: usize = 256;
const MAX_TOPIC_BYTES: usize = 1_024;
const DEFAULT_PAGE: usize = 64;
const MAX_PAGE: usize = 256;

struct Retained {
    received_at: Instant,
    encoded_bytes: usize,
    record: payload::telemetry::Record,
}

#[derive(Default)]
struct History {
    sequence: u64,
    capacity_evictions: u64,
    encoding_drops: u64,
    retained_bytes: usize,
    records: VecDeque<Retained>,
}

impl History {
    fn ingest(
        &mut self,
        now: Instant,
        participant_id: &str,
        rollup: payload::telemetry::Rollup,
    ) -> Result<Option<payload::telemetry::Follow>> {
        self.prune(now);
        let Some(sequence) = self.sequence.checked_add(1) else {
            return Ok(None);
        };
        let truncated = u32::from(participant_id.len() > MAX_TOPIC_BYTES);
        let participant_id = bounded(participant_id);
        let (topics, overflow) = bounded_rows(rollup.topics, rollup.overflow);
        let record = payload::telemetry::Record {
            sequence,
            participant_id,
            truncated,
            window_ns: rollup.window_ns,
            step: rollup.step,
            topics,
            overflow,
        };
        let encoded_bytes = match MessagePack::encode(&record) {
            Ok(encoded) => encoded.len(),
            Err(error) => {
                self.encoding_drops = self.encoding_drops.saturating_add(1);
                tracing::debug!(
                    %error,
                    encoding_drops = self.encoding_drops,
                    "dropping an invalid telemetry rollup"
                );
                return Ok(None);
            }
        };
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
        Ok(Some(payload::telemetry::Follow {
            cursor: payload::runtime::Cursor { sequence },
            record,
        }))
    }

    fn page(
        &mut self,
        now: Instant,
        request: &payload::telemetry::SnapshotRequest,
    ) -> payload::telemetry::Snapshot {
        self.prune(now);
        let limit = if request.limit == 0 {
            DEFAULT_PAGE
        } else {
            usize::try_from(request.limit)
                .unwrap_or(MAX_PAGE)
                .min(MAX_PAGE)
        };
        let matches = |retained: &&Retained| {
            request
                .participant_id
                .as_ref()
                .is_none_or(|wanted| retained.record.participant_id == *wanted)
        };
        let mut records: Vec<_> = self
            .records
            .iter()
            .rev()
            .filter(|retained| {
                request
                    .before_sequence
                    .is_none_or(|before| retained.record.sequence < before)
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
        payload::telemetry::Snapshot {
            cursor: payload::runtime::Cursor {
                sequence: self.sequence,
            },
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

    fn pop_front(&mut self, capacity: bool) {
        if let Some(record) = self.records.pop_front() {
            self.retained_bytes = self.retained_bytes.saturating_sub(record.encoded_bytes);
            self.capacity_evictions = self.capacity_evictions.saturating_add(u64::from(capacity));
        }
    }
}

fn bounded(value: &str) -> String {
    let mut end = value.len().min(MAX_TOPIC_BYTES);
    while !value.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    value[..end].to_string()
}

fn bounded_rows(
    rows: Vec<payload::runtime::Topic>,
    overflow: Option<payload::runtime::Topic>,
) -> (
    Vec<payload::runtime::Topic>,
    Option<payload::runtime::Topic>,
) {
    let mut aggregated = BTreeMap::new();
    for mut row in rows {
        row.topic = bounded(&row.topic);
        let key = (row.topic.clone(), row.direction, row.buffer_kind);
        match aggregated.get_mut(&key) {
            Some(existing) => merge(existing, &row),
            None => {
                aggregated.insert(key, row);
            }
        }
    }
    let mut rows: Vec<_> = aggregated.into_values().collect();
    let mut overflow = overflow;
    if rows.len() > MAX_TOPIC_ROWS {
        let tail = rows.split_off(MAX_TOPIC_ROWS);
        let mut folded = overflow.take().unwrap_or_else(empty_overflow);
        for row in &tail {
            merge(&mut folded, row);
        }
        folded.topic.clear();
        folded.direction = payload::runtime::Direction::Mixed;
        folded.buffer_kind = payload::runtime::BufferKind::Mixed;
        folded.overflowed_rows = folded
            .overflowed_rows
            .saturating_add(u32::try_from(tail.len()).unwrap_or(u32::MAX));
        overflow = Some(folded);
    }
    (rows, overflow)
}

fn merge(target: &mut payload::runtime::Topic, source: &payload::runtime::Topic) {
    target.count = target.count.saturating_add(source.count);
    target.rate_millihz = target.rate_millihz.saturating_add(source.rate_millihz);
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
    target.timeline_filtered = target
        .timeline_filtered
        .saturating_add(source.timeline_filtered);
    target.overflowed_rows = target
        .overflowed_rows
        .saturating_add(source.overflowed_rows);
}

fn empty_overflow() -> payload::runtime::Topic {
    payload::runtime::Topic {
        topic: String::new(),
        direction: payload::runtime::Direction::Mixed,
        buffer_kind: payload::runtime::BufferKind::Mixed,
        count: 0,
        rate_millihz: 0,
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

pub(super) async fn run(bus: BusHandle) -> Result<()> {
    let history = Arc::new(Mutex::new(History::default()));
    let follow = StreamPublisher::new(
        bus.clone(),
        &supervisor::topic::owner().telemetry().follow(),
    )?;
    let rollups =
        StreamReceiver::new(&bus, &supervisor::topic::client().telemetry().rollup()).await?;
    let mut tasks = JoinSet::new();

    let ingest_history = Arc::clone(&history);
    tasks.spawn(async move {
        loop {
            let observed = match rollups.recv_event().await? {
                StreamEvent::Item(observed) | StreamEvent::Gap { item: observed, .. } => observed,
            };
            let Some(source) = observed.metadata.source.participant_source() else {
                continue;
            };
            let retained = ingest_history
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .ingest(Instant::now(), source.participant.as_str(), observed.body)?;
            if let Some(retained) = retained
                && let Err(error) = follow.send(retained)
            {
                tracing::debug!(%error, "telemetry follow publication was dropped");
            }
        }
        #[allow(unreachable_code)]
        Ok::<(), anyhow::Error>(())
    });

    let query_history = Arc::clone(&history);
    let query_bus = bus.clone();
    tasks.spawn(async move {
        let server =
            super::declare::<supervisor::endpoint::telemetry::SnapshotEndpoint>(&query_bus).await?;
        loop {
            let incoming = server.recv().await?;
            let request: payload::telemetry::SnapshotRequest =
                match super::decode(&incoming).await? {
                    Some(request) => request,
                    None => continue,
                };
            let snapshot = query_history
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .page(Instant::now(), &request);
            super::reply(&incoming, &query_bus, &snapshot).await?;
        }
        #[allow(unreachable_code)]
        Ok::<(), anyhow::Error>(())
    });

    match tasks.join_next().await {
        Some(Ok(Ok(()))) => bail!("a telemetry collector task ended unexpectedly"),
        Some(Ok(Err(error))) => Err(error).context("a telemetry collector task failed"),
        Some(Err(error)) => Err(anyhow::anyhow!(
            "a telemetry collector task panicked: {error}"
        )),
        None => bail!("the telemetry collector started no tasks"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duplicate_rows_are_aggregated_and_the_tail_is_explicit() {
        let row = |index: usize| payload::runtime::Topic {
            topic: format!("topic/{index}"),
            ..empty_overflow()
        };
        let mut input: Vec<_> = (0..=MAX_TOPIC_ROWS).map(row).collect();
        input.push(row(0));
        let (rows, overflow) = bounded_rows(input, None);
        assert_eq!(rows.len(), MAX_TOPIC_ROWS);
        assert_eq!(rows[0].count, 0);
        assert_eq!(overflow.expect("tail is explicit").overflowed_rows, 1);
    }
}
