//! The supervisor's bounded log collector.
//!
//! `tool/log` used to retain participant logs and serve them back
//! (organization#978). The supervisor already supervises every participant, so
//! it collects their structured logs itself and answers
//! `supervisor/log/{snapshot,follow}` from its own bounded history.
//!
//! The retention behaviour is a faithful move, not a redesign: same 1,000-record
//! ring, same per-component byte budgets, same cursor and drop accounting. The
//! bounds exist so a complete snapshot stays under the bus body ceiling even
//! when every producer event arrives at its own maximum size.

use anyhow::{Context, Result};
use phoxal_api::v0_1 as api;
use phoxal_bus::{
    Bus, Codec, ContractBody, DiagnosticPublisher, MessagePack, QueryFailure, Subscribe,
    Subscriber, Topic,
};
use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};

const RETAINED_LOG_EVENTS: usize = 1_000;
const INGEST_QUEUE_DEPTH: usize = 1_024;
/// Keep the complete 1,000-record snapshot comfortably below the bus's 16 MiB
/// body ceiling even when every producer event reaches its own larger limit.
const MAX_RETAINED_RECORD_TEXT_BYTES: usize = 8 * 1_024;
const MAX_PARTICIPANT_ID_BYTES: usize = 512;
const MAX_TARGET_BYTES: usize = 256;
const MAX_MESSAGE_BYTES: usize = 16 * 1_024;
const MAX_FIELD_NAME_BYTES: usize = 128;
const MAX_FIELD_VALUE_BYTES: usize = 8 * 1_024;
const MAX_RETAINED_FIELDS: usize = 64;
#[cfg(test)]
const MAX_SNAPSHOT_WIRE_BYTES: usize = 16 * 1_024 * 1_024;

struct LogHistory {
    generation: String,
    sequence: u64,
    records: VecDeque<api::supervisor::log::Record>,
    ingest_dropped: u64,
}

impl LogHistory {
    fn new(generation: String) -> Self {
        Self {
            generation,
            sequence: 0,
            records: VecDeque::with_capacity(RETAINED_LOG_EVENTS),
            ingest_dropped: 0,
        }
    }

    fn ingest(
        &mut self,
        participant_id: String,
        event: api::logs::Event,
        ingest_dropped: u64,
    ) -> api::supervisor::log::Follow {
        self.ingest_dropped = self.ingest_dropped.max(ingest_dropped);
        self.sequence = self
            .sequence
            .checked_add(1)
            .expect("log ingest sequence exhausted");
        let record = retained_record(self.sequence, participant_id, event);
        self.records.push_back(record.clone());
        if self.records.len() > RETAINED_LOG_EVENTS {
            self.records.pop_front();
        }
        api::supervisor::log::Follow {
            cursor: self.cursor(),
            ingest_dropped: self.ingest_dropped,
            record,
        }
    }

    fn cursor(&self) -> api::tool::Cursor {
        api::tool::Cursor {
            generation: self.generation.clone(),
            sequence: self.sequence,
        }
    }

    fn snapshot(&self) -> api::supervisor::log::Snapshot {
        api::supervisor::log::Snapshot {
            cursor: self.cursor(),
            ingest_dropped: self.ingest_dropped,
            records: self.records.iter().cloned().collect(),
        }
    }

    fn snapshot_with_ingest_dropped(
        &mut self,
        ingest_dropped: u64,
    ) -> api::supervisor::log::Snapshot {
        self.ingest_dropped = self.ingest_dropped.max(ingest_dropped);
        self.snapshot()
    }
}

fn retained_record(
    sequence: u64,
    participant_id: String,
    event: api::logs::Event,
) -> api::supervisor::log::Record {
    let mut remaining_text_bytes = MAX_RETAINED_RECORD_TEXT_BYTES;
    let mut retained_truncations = event.truncated;
    // BusMetadata is the attribution authority. The wildcard topic is only
    // the selection surface: the typed Subscriber intentionally exposes the
    // decoded body plus verified metadata, not the concrete matched key.
    let participant_id = retained_component(
        &participant_id,
        MAX_PARTICIPANT_ID_BYTES,
        &mut remaining_text_bytes,
        &mut retained_truncations,
    );
    let target = retained_component(
        &event.target,
        MAX_TARGET_BYTES,
        &mut remaining_text_bytes,
        &mut retained_truncations,
    );
    let message = retained_component(
        &event.message,
        MAX_MESSAGE_BYTES,
        &mut remaining_text_bytes,
        &mut retained_truncations,
    );
    let mut fields = BTreeMap::new();
    for (name, value) in event.fields {
        if fields.len() >= MAX_RETAINED_FIELDS {
            retained_truncations = retained_truncations.saturating_add(1);
            continue;
        }
        // Field names are identifiers, not display text. Never mutate one:
        // truncating two distinct names (or truncating after the shared budget
        // is exhausted) can collapse them onto the same BTreeMap key and
        // silently overwrite a retained value. A genuinely empty source name
        // remains legal because it consumes no budget and is not synthesized.
        if name.len() > MAX_FIELD_NAME_BYTES || name.len() > remaining_text_bytes {
            retained_truncations = retained_truncations.saturating_add(1);
            continue;
        }
        remaining_text_bytes -= name.len();
        let value = match value {
            api::logs::LogValue::Bool(value) => api::supervisor::log::LogValue::Bool(value),
            api::logs::LogValue::I64(value) => api::supervisor::log::LogValue::I64(value),
            api::logs::LogValue::U64(value) => api::supervisor::log::LogValue::U64(value),
            api::logs::LogValue::F64(value) => api::supervisor::log::LogValue::F64(value),
            api::logs::LogValue::String(value) => {
                api::supervisor::log::LogValue::String(retained_component(
                    &value,
                    MAX_FIELD_VALUE_BYTES,
                    &mut remaining_text_bytes,
                    &mut retained_truncations,
                ))
            }
        };
        fields.insert(name, value);
    }
    api::supervisor::log::Record {
        sequence,
        participant_id,
        source_sequence: event.seq,
        time: api::supervisor::log::Timestamp {
            unix_seconds: event.time.unix_seconds,
            nanos: event.time.nanos,
        },
        level: match event.level {
            api::logs::Level::Error => api::supervisor::log::Level::Error,
            api::logs::Level::Warn => api::supervisor::log::Level::Warn,
            api::logs::Level::Info => api::supervisor::log::Level::Info,
            api::logs::Level::Debug => api::supervisor::log::Level::Debug,
            api::logs::Level::Trace => api::supervisor::log::Level::Trace,
        },
        target,
        message,
        fields,
        dropped: event.dropped,
        truncated: retained_truncations,
    }
}

fn retained_component(
    value: &str,
    component_limit: usize,
    remaining: &mut usize,
    truncations: &mut u32,
) -> String {
    let mut end = value.len().min(component_limit).min(*remaining);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    *remaining = remaining.saturating_sub(end);
    if end < value.len() {
        *truncations = truncations.saturating_add(1);
    }
    value[..end].to_string()
}

/// Collect participant logs on `bus` and serve them back until the session ends.
///
/// Two concurrent halves, exactly as the deleted tool had them: an ingest loop
/// draining the wildcard `logs/*` subscription into the bounded history and
/// republishing each retained record on `follow`, and a query loop answering
/// `snapshot` from that same history.
pub(crate) async fn serve_logs(bus: &Bus, generation: String) -> Result<()> {
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
    let follow = DiagnosticPublisher::<api::supervisor::log::Follow>::new(
        bus.clone(),
        &Topic::new_static(<api::supervisor::log::Follow as ContractBody>::TOPIC),
    )
    .context("failed to declare the log follow publisher")?;
    let snapshots = bus
        .declare_server(<api::supervisor::log::SnapshotRequest as ContractBody>::TOPIC)
        .await
        .context("failed to declare the log snapshot server")?;

    // This clone exists only for the non-destructive `dropped()` counter.
    // Subscriber clones compete for one destructive queue, so the query half
    // must never call recv()/try_recv() on it.
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
            let participant_id = received.metadata.participant;
            // The subscription-local cumulative count, not the aggregate
            // BusHealth counter, so every retained item and snapshot exposes
            // any loss that happened before this receive.
            let ingest_dropped = logs.dropped();
            let item = history
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .ingest(participant_id, received.body, ingest_dropped);
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
            if let Err(error) = MessagePack::decode::<api::supervisor::log::SnapshotRequest>(
                &incoming.request_bytes().unwrap_or_default(),
            ) {
                let _ = incoming
                    .reply_err(&QueryFailure::invalid_argument(format!(
                        "decode log snapshot request: {error}"
                    )))
                    .await;
                continue;
            }
            let snapshot = query_history
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .snapshot_with_ingest_dropped(query_logs.dropped());
            match MessagePack::encode(&snapshot) {
                Ok(payload) => {
                    if let Err(error) = incoming.reply(bus, payload).await {
                        tracing::debug!("log snapshot reply failed: {error}");
                    }
                }
                Err(error) => {
                    let _ = incoming
                        .reply_err(&QueryFailure::internal(format!(
                            "encode log snapshot: {error}"
                        )))
                        .await;
                }
            }
        }
    };

    tracing::debug!("supervisor is collecting participant logs");
    tokio::join!(ingest, serve);
    Ok(())
}

/// A fresh opaque generation for this collector's cursor.
///
/// Consumers compare generations for equality only, never parse or order them,
/// so a restart simply produces an unrelated identity and invalidates any cursor
/// a client was holding.
pub(crate) fn log_generation() -> Result<String> {
    let mut random = [0_u8; 16];
    getrandom::fill(&mut random)
        .map_err(|error| anyhow::anyhow!("OS entropy unavailable for log generation: {error}"))?;
    Ok(random.iter().map(|byte| format!("{byte:02x}")).collect())
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

    #[test]
    fn retention_is_exactly_one_thousand_newest_records() {
        let mut history = LogHistory::new("generation-a".to_string());
        for sequence in 1..=1_005 {
            history.ingest("drive".to_string(), event(sequence, "sample"), 0);
        }
        let snapshot = history.snapshot();
        assert_eq!(snapshot.cursor.sequence, 1_005);
        assert_eq!(snapshot.records.len(), RETAINED_LOG_EVENTS);
        assert_eq!(snapshot.records.first().unwrap().sequence, 6);
        assert_eq!(snapshot.records.last().unwrap().sequence, 1_005);
    }

    #[test]
    fn tool_sequence_is_global_while_source_sequence_is_preserved() {
        let mut history = LogHistory::new("generation-a".to_string());
        let first = history.ingest("drive".to_string(), event(41, "one"), 0);
        let second = history.ingest("map".to_string(), event(7, "two"), 0);
        assert_eq!(first.cursor.sequence, 1);
        assert_eq!(first.record.source_sequence, 41);
        assert_eq!(second.cursor.sequence, 2);
        assert_eq!(second.record.source_sequence, 7);
    }

    #[test]
    fn snapshot_then_buffered_follow_closes_the_query_race() {
        let mut history = LogHistory::new("generation-a".to_string());
        let _first = history.ingest("drive".to_string(), event(1, "one"), 0);
        let buffered_covered = history.ingest("drive".to_string(), event(2, "two"), 0);
        let snapshot = history.snapshot();
        let buffered_newer = history.ingest("drive".to_string(), event(3, "three"), 0);

        let replay = [buffered_covered, buffered_newer]
            .into_iter()
            .filter(|follow| {
                follow.cursor.generation == snapshot.cursor.generation
                    && follow.cursor.sequence > snapshot.cursor.sequence
            })
            .collect::<Vec<_>>();
        assert_eq!(snapshot.records.len(), 2);
        assert_eq!(replay.len(), 1);
        assert_eq!(replay[0].record.message, "three");
    }

    #[test]
    fn generation_change_and_sequence_gap_are_requery_conditions() {
        let installed = api::tool::Cursor {
            generation: "generation-a".to_string(),
            sequence: 8,
        };
        let restarted = api::tool::Cursor {
            generation: "generation-b".to_string(),
            sequence: 1,
        };
        let gap = api::tool::Cursor {
            generation: "generation-a".to_string(),
            sequence: 10,
        };
        assert!(requires_requery(&installed, &restarted));
        assert!(requires_requery(&installed, &gap));
        assert!(!requires_requery(
            &installed,
            &api::tool::Cursor {
                generation: "generation-a".to_string(),
                sequence: 9,
            }
        ));
    }

    #[test]
    fn maximal_retained_snapshot_stays_inside_the_bus_body_ceiling() {
        let field_name_bytes = (0..MAX_RETAINED_FIELDS)
            .map(|index| format!("f{index:02}").len())
            .sum::<usize>();
        let message_bytes = MAX_RETAINED_RECORD_TEXT_BYTES
            - MAX_PARTICIPANT_ID_BYTES
            - MAX_TARGET_BYTES
            - field_name_bytes;
        let oversized = api::logs::Event {
            seq: 1,
            time: api::logs::Timestamp {
                unix_seconds: 1,
                nanos: 2,
            },
            level: api::logs::Level::Info,
            target: "t".repeat(256),
            message: "m".repeat(message_bytes),
            fields: (0..MAX_RETAINED_FIELDS)
                .map(|index| (format!("f{index:02}"), api::logs::LogValue::U64(u64::MAX)))
                .collect(),
            dropped: 0,
            truncated: 0,
        };
        let mut history = LogHistory::new("generation-a".to_string());
        for sequence in 0..RETAINED_LOG_EVENTS {
            let mut event = oversized.clone();
            event.seq = sequence as u64;
            history.ingest("p".repeat(MAX_PARTICIPANT_ID_BYTES), event, 0);
        }

        let snapshot = history.snapshot();
        let record = &snapshot.records[0];
        let retained_text_bytes = record.participant_id.len()
            + record.target.len()
            + record.message.len()
            + record.fields.keys().map(String::len).sum::<usize>();
        assert_eq!(retained_text_bytes, MAX_RETAINED_RECORD_TEXT_BYTES);
        let encoded = MessagePack::encode(&snapshot).unwrap();
        assert_eq!(record.fields.len(), MAX_RETAINED_FIELDS);
        assert!(
            encoded.len() <= MAX_SNAPSHOT_WIRE_BYTES,
            "{}-byte retained log snapshot exceeds the bus body ceiling",
            encoded.len()
        );
    }

    #[test]
    fn ingest_loss_is_cumulative_and_visible_on_follow_and_snapshot() {
        let mut history = LogHistory::new("generation-a".to_string());
        let first = history.ingest("drive".to_string(), event(1, "one"), 3);
        let second = history.ingest("drive".to_string(), event(2, "two"), 7);
        assert_eq!(first.ingest_dropped, 3);
        assert_eq!(second.ingest_dropped, 7);
        assert_eq!(history.snapshot().ingest_dropped, 7);
    }

    #[test]
    fn retained_record_preserves_the_supplied_participant_id() {
        let mut history = LogHistory::new("generation-a".to_string());
        let follow = history.ingest("metadata-source".to_string(), event(1, "one"), 0);
        assert_eq!(follow.record.participant_id, "metadata-source");
    }

    #[test]
    fn hostile_components_are_bounded_before_the_shared_text_budget() {
        let record = retained_record(
            1,
            "p".repeat(MAX_PARTICIPANT_ID_BYTES + 1),
            api::logs::Event {
                seq: 1,
                time: api::logs::Timestamp {
                    unix_seconds: 1,
                    nanos: 2,
                },
                level: api::logs::Level::Info,
                target: "t".repeat(MAX_TARGET_BYTES + 1),
                message: "m".repeat(MAX_MESSAGE_BYTES + 1),
                fields: BTreeMap::new(),
                dropped: 0,
                truncated: 0,
            },
        );
        assert_eq!(record.participant_id.len(), MAX_PARTICIPANT_ID_BYTES);
        assert_eq!(record.target.len(), MAX_TARGET_BYTES);
        assert_eq!(
            record.participant_id.len() + record.target.len() + record.message.len(),
            MAX_RETAINED_RECORD_TEXT_BYTES
        );
        assert!(record.truncated >= 3);

        let fields = [(
            "n".repeat(MAX_FIELD_NAME_BYTES + 1),
            api::logs::LogValue::U64(1),
        )]
        .into_iter()
        .collect();
        let field_record = retained_record(
            2,
            String::new(),
            api::logs::Event {
                seq: 2,
                time: api::logs::Timestamp {
                    unix_seconds: 1,
                    nanos: 2,
                },
                level: api::logs::Level::Info,
                target: String::new(),
                message: String::new(),
                fields,
                dropped: 0,
                truncated: 0,
            },
        );
        assert!(field_record.fields.is_empty());
        assert_eq!(field_record.truncated, 1);
    }

    #[test]
    fn exhausted_budget_and_overlong_names_skip_fields_without_key_collisions() {
        let overlong_prefix = "x".repeat(MAX_FIELD_NAME_BYTES);
        let fields = [
            ("first".to_string(), api::logs::LogValue::U64(1)),
            ("second".to_string(), api::logs::LogValue::U64(2)),
            (format!("{overlong_prefix}-a"), api::logs::LogValue::U64(3)),
            (format!("{overlong_prefix}-b"), api::logs::LogValue::U64(4)),
        ]
        .into_iter()
        .collect();
        let record = retained_record(
            3,
            String::new(),
            api::logs::Event {
                seq: 3,
                time: api::logs::Timestamp {
                    unix_seconds: 1,
                    nanos: 2,
                },
                level: api::logs::Level::Info,
                target: String::new(),
                // Legal for the producer but larger than tool-log's shared
                // retained budget, so no synthetic empty field key may appear.
                message: "m".repeat(MAX_RETAINED_RECORD_TEXT_BYTES + 1),
                fields,
                dropped: 0,
                truncated: 0,
            },
        );
        assert_eq!(record.message.len(), MAX_RETAINED_RECORD_TEXT_BYTES);
        assert!(record.fields.is_empty());
        assert!(!record.fields.contains_key(""));
        // One message truncation plus all four skipped fields.
        assert_eq!(record.truncated, 5);

        let genuinely_empty = retained_record(
            4,
            String::new(),
            api::logs::Event {
                seq: 4,
                time: api::logs::Timestamp {
                    unix_seconds: 1,
                    nanos: 2,
                },
                level: api::logs::Level::Info,
                target: String::new(),
                message: String::new(),
                fields: [(String::new(), api::logs::LogValue::U64(9))]
                    .into_iter()
                    .collect(),
                dropped: 0,
                truncated: 0,
            },
        );
        assert_eq!(
            genuinely_empty.fields.get(""),
            Some(&api::supervisor::log::LogValue::U64(9))
        );
    }

    #[test]
    fn log_generation_is_opaque_and_unique_per_call() {
        let first = log_generation().unwrap();
        let second = log_generation().unwrap();
        assert_eq!(first.len(), 32);
        assert!(first.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert_ne!(first, second);
    }

    fn requires_requery(installed: &api::tool::Cursor, next: &api::tool::Cursor) -> bool {
        installed.generation != next.generation
            || next.sequence != installed.sequence.saturating_add(1)
    }
}
