//! The supervisor's bounded log collector.
//!
//! The supervisor already supervises every participant, so it collects their
//! structured logs itself and answers `supervisor/logs/{snapshot,follow}` from
//! its own bounded history - there is no log tool to run, and no second process
//! to keep alive.
//!
//! The retention is the one the tool had: a 1,000-record ring, a per-record
//! shared text budget, and a cursor whose generation is opaque. What the wire
//! contract adds on top is **paging**: a snapshot answers one backward page,
//! filtered to one participant or to all, so a client renders a screenful
//! without the daemon ever encoding the whole ring.
//!
//! Two loss counters, deliberately distinct: `LogRecord::dropped` is what a
//! producer lost before publishing, and `ingest_dropped` is what *this*
//! subscriber lost. An increase in the second is unrecoverable source loss.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use phoxal_api::v0_1 as api;
use phoxal_bus::{
    Bus, Codec, ContractBody, DiagnosticPublisher, MessagePack, QueryFailure, Subscribe,
    Subscriber, Topic,
};
use phoxal_supervisor_api::{
    Cursor, LogLevel, LogRecord, LogText, LogValue, Name, WallTime, supervisor,
};

const RETAINED_LOG_RECORDS: usize = 1_000;
const INGEST_QUEUE_DEPTH: usize = 1_024;
/// Total retained text per record. Keeps a maximal page comfortably below the
/// bus body ceiling even when every producer publishes at its own limit.
const MAX_RETAINED_RECORD_TEXT_BYTES: usize = 8 * 1_024;
const MAX_RETAINED_FIELDS: usize = 64;
const DEFAULT_QUERY_RECORDS: usize = 200;
const MAX_QUERY_RECORDS: usize = RETAINED_LOG_RECORDS;

pub(crate) struct LogHistory {
    generation: Name,
    sequence: u64,
    records: VecDeque<LogRecord>,
    ingest_dropped: u64,
}

impl LogHistory {
    pub(crate) fn new(generation: Name) -> Self {
        Self {
            generation,
            sequence: 0,
            records: VecDeque::with_capacity(RETAINED_LOG_RECORDS),
            ingest_dropped: 0,
        }
    }

    pub(crate) fn ingest(
        &mut self,
        participant: &str,
        event: api::logs::Event,
        ingest_dropped: u64,
    ) -> supervisor::logs::Follow {
        self.ingest_dropped = self.ingest_dropped.max(ingest_dropped);
        self.sequence = self
            .sequence
            .checked_add(1)
            .expect("log ingest sequence exhausted");
        let record = retained_record(self.sequence, participant, event);
        self.records.push_back(record.clone());
        if self.records.len() > RETAINED_LOG_RECORDS {
            self.records.pop_front();
        }
        supervisor::logs::Follow::V0 {
            cursor: self.cursor(),
            ingest_dropped: self.ingest_dropped,
            record,
        }
    }

    fn cursor(&self) -> Cursor {
        Cursor {
            generation: self.generation.clone(),
            sequence: self.sequence,
        }
    }

    /// One backward page, oldest-first within the page.
    pub(crate) fn page(
        &mut self,
        request: &supervisor::logs::SnapshotRequest,
        ingest_dropped: u64,
    ) -> supervisor::logs::Snapshot {
        self.ingest_dropped = self.ingest_dropped.max(ingest_dropped);
        let supervisor::logs::SnapshotRequest::V0 {
            participant,
            limit,
            before_sequence,
        } = request;
        let limit = page_size(*limit);
        let matches = |record: &&LogRecord| {
            participant
                .as_ref()
                .is_none_or(|wanted| record.participant == *wanted)
        };
        let mut records: Vec<_> = self
            .records
            .iter()
            .rev()
            .filter(|record| before_sequence.is_none_or(|before| record.sequence < before))
            .filter(matches)
            .take(limit)
            .cloned()
            .collect();
        records.reverse();
        // `Some` only when retained matching history actually continues below
        // this page; `None` means the client has reached the end of what the
        // supervisor still holds.
        let next_before_sequence = records.first().and_then(|first| {
            self.records
                .iter()
                .filter(matches)
                .any(|record| record.sequence < first.sequence)
                .then_some(first.sequence)
        });
        supervisor::logs::Snapshot::V0 {
            cursor: self.cursor(),
            ingest_dropped: self.ingest_dropped,
            records,
            next_before_sequence,
        }
    }
}

/// Zero selects the supervisor's own default; anything larger than the ring is
/// clamped to it, so a client cannot ask for more than exists.
fn page_size(requested: u32) -> usize {
    if requested == 0 {
        return DEFAULT_QUERY_RECORDS;
    }
    usize::try_from(requested)
        .unwrap_or(MAX_QUERY_RECORDS)
        .min(MAX_QUERY_RECORDS)
}

fn retained_record(sequence: u64, participant: &str, event: api::logs::Event) -> LogRecord {
    let mut budget = Budget::new(MAX_RETAINED_RECORD_TEXT_BYTES, event.truncated);
    let participant = budget.name(participant);
    let target = budget.text(&event.target);
    let message = budget.text(&event.message);
    let mut fields = BTreeMap::new();
    for (name, value) in event.fields {
        if fields.len() >= MAX_RETAINED_FIELDS {
            budget.truncations = budget.truncations.saturating_add(1);
            continue;
        }
        // A field name is an identifier, never display text: truncating one
        // could collapse two distinct names onto the same key and silently
        // overwrite a retained value, so an oversized name skips its whole
        // field instead. A genuinely empty name is legal - it costs no budget
        // and is not synthesized.
        if name.len() > Name::MAX_BYTES || name.len() > budget.remaining {
            budget.truncations = budget.truncations.saturating_add(1);
            continue;
        }
        budget.remaining -= name.len();
        let value = match value {
            api::logs::LogValue::Bool(value) => LogValue::Bool(value),
            api::logs::LogValue::I64(value) => LogValue::I64(value),
            api::logs::LogValue::U64(value) => LogValue::U64(value),
            api::logs::LogValue::F64(value) => LogValue::F64(value),
            api::logs::LogValue::String(value) => LogValue::String(budget.text(&value)),
        };
        fields.insert(Name::new(name), value);
    }
    LogRecord {
        sequence,
        participant,
        source_sequence: event.seq,
        time: WallTime {
            unix_seconds: event.time.unix_seconds,
            nanos: event.time.nanos,
        },
        level: match event.level {
            api::logs::Level::Error => LogLevel::Error,
            api::logs::Level::Warn => LogLevel::Warn,
            api::logs::Level::Info => LogLevel::Info,
            api::logs::Level::Debug => LogLevel::Debug,
            api::logs::Level::Trace => LogLevel::Trace,
        },
        target,
        message,
        fields,
        dropped: event.dropped,
        truncated: budget.truncations,
    }
}

/// The shared text budget one record's components draw from, in order.
struct Budget {
    remaining: usize,
    truncations: u32,
}

impl Budget {
    const fn new(remaining: usize, truncations: u32) -> Self {
        Self {
            remaining,
            truncations,
        }
    }

    fn name(&mut self, value: &str) -> Name {
        Name::new(self.take(value, Name::MAX_BYTES))
    }

    fn text(&mut self, value: &str) -> LogText {
        LogText::new(self.take(value, LogText::MAX_BYTES))
    }

    fn take<'a>(&mut self, value: &'a str, component_limit: usize) -> &'a str {
        let mut end = value.len().min(component_limit).min(self.remaining);
        while !value.is_char_boundary(end) {
            end -= 1;
        }
        self.remaining -= end;
        if end < value.len() {
            self.truncations = self.truncations.saturating_add(1);
        }
        &value[..end]
    }
}

/// Collect participant logs on `bus` and serve them back until the session ends.
pub(crate) async fn run(bus: &Bus, generation: Name) -> Result<()> {
    let history = Arc::new(Mutex::new(LogHistory::new(generation)));

    let logs = Subscriber::<api::logs::Event>::new(
        bus,
        &Topic::<Subscribe<api::logs::Event>>::new_static(
            <api::logs::Event as ContractBody>::TOPIC,
        ),
        INGEST_QUEUE_DEPTH,
    )
    .await
    .context("failed to subscribe to participant logs")?;
    let follow = DiagnosticPublisher::<supervisor::logs::Follow>::new(
        bus.clone(),
        &supervisor::topic::owner().logs().follow(),
    )
    .context("failed to declare the log follow publisher")?;
    let snapshots = super::declare(bus, supervisor::topic::owner().logs().snapshot().key()).await?;

    // This clone exists only for the non-destructive `dropped()` counter:
    // subscriber clones compete for one destructive queue, so the query half
    // must never receive on it.
    let query_logs = logs.clone();
    let query_history = Arc::clone(&history);

    let ingest = async {
        loop {
            let received = match logs.recv().await {
                Ok(received) => received,
                Err(error) => {
                    tracing::debug!("log ingest stopped: {error}");
                    return;
                }
            };
            // BusMetadata is the attribution authority - never the matched key.
            let participant = received.metadata.participant;
            let item = history
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .ingest(&participant, received.body, logs.dropped());
            if let Err(error) = follow.publish(item) {
                tracing::debug!("log follow publish failed: {error}");
            }
        }
    };

    let serve = async {
        loop {
            let incoming = match snapshots.recv().await {
                Ok(incoming) => incoming,
                Err(error) => {
                    tracing::debug!("log snapshot server stopped: {error}");
                    return;
                }
            };
            let request = match MessagePack::decode::<supervisor::logs::SnapshotRequest>(
                &incoming.request_bytes().unwrap_or_default(),
            ) {
                Ok(request) => request,
                Err(error) => {
                    let _ = incoming
                        .reply_err(&QueryFailure::invalid_argument(format!(
                            "malformed logs/snapshot request: {error}"
                        )))
                        .await;
                    continue;
                }
            };
            let page = query_history
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .page(&request, query_logs.dropped());
            match MessagePack::encode(&page) {
                Ok(payload) => {
                    if let Err(error) = incoming.reply(bus, payload).await {
                        tracing::debug!("log snapshot reply failed: {error}");
                    }
                }
                Err(error) => {
                    let _ = incoming
                        .reply_err(&QueryFailure::internal(format!(
                            "failed to encode a log page: {error}"
                        )))
                        .await;
                }
            }
        }
    };

    tracing::debug!("the supervisor is collecting participant logs");
    tokio::join!(ingest, serve);
    Ok(())
}

/// A fresh opaque generation for this collector's cursor.
///
/// Consumers compare generations for equality only, never parse or order them,
/// so a restart simply produces an unrelated identity and invalidates whatever
/// cursor a client was holding.
pub(crate) fn generation() -> Result<Name> {
    let mut random = [0_u8; 16];
    getrandom::fill(&mut random).map_err(|error| {
        anyhow::anyhow!("OS entropy unavailable for a cursor generation: {error}")
    })?;
    Ok(Name::new(
        random
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(source_sequence: u64, message: &str) -> api::logs::Event {
        api::logs::Event {
            seq: source_sequence,
            time: api::logs::Timestamp {
                unix_seconds: 1,
                nanos: 2,
            },
            level: api::logs::Level::Info,
            target: "test".to_string(),
            message: message.to_string(),
            fields: BTreeMap::new(),
            dropped: 0,
            truncated: 0,
        }
    }

    fn history() -> LogHistory {
        LogHistory::new(Name::new("generation-a"))
    }

    fn request(
        participant: Option<&str>,
        limit: u32,
        before_sequence: Option<u64>,
    ) -> supervisor::logs::SnapshotRequest {
        supervisor::logs::SnapshotRequest::V0 {
            participant: participant.map(Name::new),
            limit,
            before_sequence,
        }
    }

    #[test]
    fn retention_keeps_exactly_the_newest_thousand_records() {
        let mut history = history();
        for sequence in 1..=1_005 {
            history.ingest("drive", event(sequence, "sample"), 0);
        }
        let supervisor::logs::Snapshot::V0 {
            cursor, records, ..
        } = history.page(&request(None, MAX_QUERY_RECORDS as u32, None), 0);
        assert_eq!(cursor.sequence, 1_005);
        assert_eq!(records.len(), RETAINED_LOG_RECORDS);
        assert_eq!(records.first().expect("oldest").sequence, 6);
        assert_eq!(records.last().expect("newest").sequence, 1_005);
    }

    #[test]
    fn a_page_walks_backwards_and_stops_when_the_history_runs_out() {
        let mut history = history();
        for sequence in 1..=5 {
            history.ingest("drive", event(sequence, "sample"), 0);
        }

        let supervisor::logs::Snapshot::V0 {
            records,
            next_before_sequence,
            ..
        } = history.page(&request(None, 2, None), 0);
        assert_eq!(
            records
                .iter()
                .map(|record| record.sequence)
                .collect::<Vec<_>>(),
            [4, 5],
            "a page is oldest-first within itself"
        );
        assert_eq!(next_before_sequence, Some(4));

        let supervisor::logs::Snapshot::V0 {
            records,
            next_before_sequence,
            ..
        } = history.page(&request(None, 10, next_before_sequence), 0);
        assert_eq!(
            records
                .iter()
                .map(|record| record.sequence)
                .collect::<Vec<_>>(),
            [1, 2, 3]
        );
        assert_eq!(
            next_before_sequence, None,
            "the retained history is exhausted, so there is no next page"
        );
    }

    #[test]
    fn a_participant_filter_selects_and_pages_only_that_participants_records() {
        let mut history = history();
        for sequence in 1..=6 {
            let participant = if sequence % 2 == 0 { "drive" } else { "brain" };
            history.ingest(participant, event(sequence, "sample"), 0);
        }
        let supervisor::logs::Snapshot::V0 {
            records,
            next_before_sequence,
            ..
        } = history.page(&request(Some("drive"), 2, None), 0);
        assert!(
            records
                .iter()
                .all(|record| record.participant.as_str() == "drive")
        );
        assert_eq!(
            records
                .iter()
                .map(|record| record.sequence)
                .collect::<Vec<_>>(),
            [4, 6]
        );
        assert_eq!(
            next_before_sequence,
            Some(4),
            "the continuation considers only matching records"
        );

        let supervisor::logs::Snapshot::V0 {
            records,
            next_before_sequence,
            ..
        } = history.page(&request(Some("drive"), 2, next_before_sequence), 0);
        assert_eq!(
            records
                .iter()
                .map(|record| record.sequence)
                .collect::<Vec<_>>(),
            [2]
        );
        assert_eq!(next_before_sequence, None);
    }

    #[test]
    fn the_supervisor_sequence_is_global_while_the_producers_own_is_preserved() {
        let mut history = history();
        let supervisor::logs::Follow::V0 { cursor, record, .. } =
            history.ingest("drive", event(41, "one"), 0);
        assert_eq!(cursor.sequence, 1);
        assert_eq!(record.source_sequence, 41);
        let supervisor::logs::Follow::V0 { cursor, record, .. } =
            history.ingest("map", event(7, "two"), 0);
        assert_eq!(cursor.sequence, 2);
        assert_eq!(record.source_sequence, 7);
        assert_eq!(record.participant.as_str(), "map");
    }

    #[test]
    fn ingest_loss_is_cumulative_and_visible_on_both_follow_and_snapshot() {
        let mut history = history();
        let supervisor::logs::Follow::V0 { ingest_dropped, .. } =
            history.ingest("drive", event(1, "one"), 3);
        assert_eq!(ingest_dropped, 3);
        let supervisor::logs::Follow::V0 { ingest_dropped, .. } =
            history.ingest("drive", event(2, "two"), 7);
        assert_eq!(ingest_dropped, 7);
        let supervisor::logs::Snapshot::V0 { ingest_dropped, .. } =
            history.page(&request(None, 0, None), 5);
        assert_eq!(ingest_dropped, 7, "a lower observation never lowers it");
    }

    #[test]
    fn hostile_components_are_bounded_and_never_collide_field_keys() {
        let record = retained_record(
            1,
            &"p".repeat(Name::MAX_BYTES + 1),
            api::logs::Event {
                seq: 1,
                time: api::logs::Timestamp {
                    unix_seconds: 1,
                    nanos: 2,
                },
                level: api::logs::Level::Info,
                target: "t".repeat(LogText::MAX_BYTES + 1),
                message: "m".repeat(MAX_RETAINED_RECORD_TEXT_BYTES + 1),
                fields: [
                    ("kept".to_string(), api::logs::LogValue::U64(1)),
                    (
                        format!("{}-a", "x".repeat(Name::MAX_BYTES)),
                        api::logs::LogValue::U64(2),
                    ),
                    (
                        format!("{}-b", "x".repeat(Name::MAX_BYTES)),
                        api::logs::LogValue::U64(3),
                    ),
                ]
                .into_iter()
                .collect(),
                dropped: 0,
                truncated: 0,
            },
        );
        assert_eq!(record.participant.as_str().len(), Name::MAX_BYTES);
        assert_eq!(record.target.as_str().len(), LogText::MAX_BYTES);
        let text = record.participant.as_str().len()
            + record.target.as_str().len()
            + record.message.as_str().len()
            + record
                .fields
                .keys()
                .map(|name| name.as_str().len())
                .sum::<usize>();
        assert!(
            text <= MAX_RETAINED_RECORD_TEXT_BYTES,
            "the shared budget bounds the whole record: {text}"
        );
        // Both overlong names were skipped whole rather than truncated onto one
        // colliding key.
        assert!(
            !record
                .fields
                .keys()
                .any(|name| name.as_str().starts_with('x'))
        );
        assert!(record.truncated >= 3);
    }

    #[test]
    fn a_generation_is_opaque_and_unique_per_call() {
        let first = generation().expect("entropy");
        let second = generation().expect("entropy");
        assert_eq!(first.as_str().len(), 32);
        assert!(first.as_str().bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert_ne!(first, second);
    }

    #[test]
    fn a_maximal_page_stays_inside_the_bus_body_ceiling() {
        const BUS_BODY_CEILING: usize = 16 * 1024 * 1024;
        let mut history = history();
        let oversized = api::logs::Event {
            seq: 1,
            time: api::logs::Timestamp {
                unix_seconds: 1,
                nanos: 2,
            },
            level: api::logs::Level::Info,
            target: "t".repeat(LogText::MAX_BYTES),
            message: "m".repeat(MAX_RETAINED_RECORD_TEXT_BYTES),
            fields: (0..MAX_RETAINED_FIELDS)
                .map(|index| (format!("f{index:02}"), api::logs::LogValue::U64(u64::MAX)))
                .collect(),
            dropped: 0,
            truncated: 0,
        };
        for sequence in 0..RETAINED_LOG_RECORDS {
            let mut event = oversized.clone();
            event.seq = sequence as u64;
            history.ingest(&"p".repeat(Name::MAX_BYTES), event, 0);
        }
        let page = history.page(&request(None, MAX_QUERY_RECORDS as u32, None), 0);
        let encoded = MessagePack::encode(&page).expect("a page encodes");
        assert!(
            encoded.len() <= BUS_BODY_CEILING,
            "a {}-byte page exceeds the bus body ceiling",
            encoded.len()
        );
    }
}
