//! Bounded participant-log retention owned by the supervisor process.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use phoxal_bus::{BusHandle, StreamEvent, StreamPublisher, StreamReceiver};
use phoxal_supervisor_api::{payload, supervisor};
use tokio::task::JoinSet;

use crate::daemon::roster::Roster;

const RETAINED_RECORDS: usize = 1_000;
const RECORD_TEXT_BYTES: usize = 8 * 1_024;
const MAX_FIELDS: usize = 64;
const DEFAULT_PAGE: usize = 64;
const MAX_PAGE: usize = 256;

#[derive(Default)]
struct History {
    sequence: u64,
    ingest_dropped: u64,
    records: VecDeque<payload::log::Record>,
}

impl History {
    fn ingest(
        &mut self,
        participant_id: &str,
        event: payload::logs::Event,
        gap: u64,
    ) -> Option<payload::log::Follow> {
        self.ingest_dropped = self.ingest_dropped.saturating_add(gap);
        let Some(sequence) = self.sequence.checked_add(1) else {
            self.ingest_dropped = self.ingest_dropped.saturating_add(1);
            return None;
        };
        self.sequence = sequence;
        let record = retain(sequence, participant_id, event);
        self.records.push_back(record.clone());
        if self.records.len() > RETAINED_RECORDS {
            self.records.pop_front();
        }
        Some(payload::log::Follow {
            cursor: payload::runtime::Cursor { sequence },
            ingest_dropped: self.ingest_dropped,
            record,
        })
    }

    fn snapshot(&self, request: &payload::log::SnapshotRequest) -> payload::log::Snapshot {
        let limit = if request.limit == 0 {
            DEFAULT_PAGE
        } else {
            usize::try_from(request.limit)
                .unwrap_or(MAX_PAGE)
                .min(MAX_PAGE)
        };
        let matches = |record: &&payload::log::Record| {
            request
                .participant_id
                .as_ref()
                .is_none_or(|wanted| record.participant_id == *wanted)
        };
        let mut records: Vec<_> = self
            .records
            .iter()
            .rev()
            .filter(|record| {
                request
                    .before_sequence
                    .is_none_or(|before| record.sequence < before)
            })
            .filter(matches)
            .take(limit)
            .cloned()
            .collect();
        records.reverse();
        let next_before_sequence = records.first().and_then(|first| {
            self.records
                .iter()
                .filter(matches)
                .any(|record| record.sequence < first.sequence)
                .then_some(first.sequence)
        });
        payload::log::Snapshot {
            cursor: payload::runtime::Cursor {
                sequence: self.sequence,
            },
            ingest_dropped: self.ingest_dropped,
            records,
            next_before_sequence,
        }
    }
}

fn retain(
    sequence: u64,
    participant_id: &str,
    event: payload::logs::Event,
) -> payload::log::Record {
    let mut remaining = RECORD_TEXT_BYTES;
    let mut truncated = event.truncated;
    let participant_id = take(participant_id, &mut remaining, &mut truncated);
    let target = take(&event.target, &mut remaining, &mut truncated);
    let message = take(&event.message, &mut remaining, &mut truncated);
    let mut fields = BTreeMap::new();
    for (name, value) in event.fields {
        if fields.len() == MAX_FIELDS || name.len() > remaining {
            truncated = truncated.saturating_add(1);
            continue;
        }
        remaining -= name.len();
        let value = match value {
            payload::logs::LogValue::String(value) => {
                payload::logs::LogValue::String(take(&value, &mut remaining, &mut truncated))
            }
            value => value,
        };
        fields.insert(name, value);
    }
    payload::log::Record {
        sequence,
        participant_id,
        source_sequence: event.seq,
        time: event.time,
        level: event.level,
        target,
        message,
        fields,
        dropped: event.dropped,
        truncated,
    }
}

fn take(value: &str, remaining: &mut usize, truncated: &mut u32) -> String {
    let mut end = value.len().min(*remaining);
    while !value.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    *remaining -= end;
    if end < value.len() {
        *truncated = truncated.saturating_add(1);
    }
    value[..end].to_string()
}

pub(super) async fn run(bus: BusHandle, roster: Roster) -> Result<()> {
    let history = Arc::new(Mutex::new(History::default()));
    let follow = StreamPublisher::new(bus.clone(), &supervisor::topic::owner().log().follow())?;
    let mut tasks = JoinSet::new();

    let query_bus = bus.clone();
    let query_history = Arc::clone(&history);
    tasks.spawn(async move {
        let server =
            super::declare::<supervisor::endpoint::log::SnapshotEndpoint>(&query_bus).await?;
        loop {
            let incoming = server.recv().await?;
            let request: payload::log::SnapshotRequest = match super::decode(&incoming).await? {
                Some(request) => request,
                None => continue,
            };
            let snapshot = query_history
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .snapshot(&request);
            super::reply(&incoming, &query_bus, &snapshot).await?;
        }
        #[allow(unreachable_code)]
        Ok::<(), anyhow::Error>(())
    });

    for entry in roster.entries() {
        let participant = entry.key.participant().clone();
        let topic = supervisor::topic::client().logs(&participant)?.topic();
        let receiver = StreamReceiver::new(&bus, &topic).await?;
        let history = Arc::clone(&history);
        let follow = follow.clone();
        tasks.spawn(async move {
            loop {
                let (observed, gap) = match receiver.recv_event().await? {
                    StreamEvent::Item(observed) => (observed, 0),
                    StreamEvent::Gap {
                        expected,
                        observed,
                        item,
                    } => (item, observed.saturating_sub(expected)),
                };
                let Some(source) = observed.metadata.source.participant_source() else {
                    continue;
                };
                if source.participant != participant {
                    continue;
                }
                let retained = history
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .ingest(participant.as_str(), observed.body, gap);
                if let Some(retained) = retained
                    && let Err(error) = follow.send(retained)
                {
                    tracing::debug!(%error, "log follow publication was dropped");
                }
            }
            #[allow(unreachable_code)]
            Ok::<(), anyhow::Error>(())
        });
    }

    match tasks.join_next().await {
        Some(Ok(Ok(()))) => bail!("a log collector task ended unexpectedly"),
        Some(Ok(Err(error))) => Err(error).context("a log collector task failed"),
        Some(Err(error)) => Err(anyhow::anyhow!("a log collector task panicked: {error}")),
        None => bail!("the log collector started no tasks"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(message: &str) -> payload::logs::Event {
        payload::logs::Event {
            seq: 7,
            time: payload::logs::Timestamp {
                unix_seconds: 1,
                nanos: 2,
            },
            level: payload::logs::Level::Info,
            target: "test".to_string(),
            message: message.to_string(),
            fields: BTreeMap::new(),
            dropped: 0,
            truncated: 0,
        }
    }

    #[test]
    fn retention_is_bounded_and_reports_ingest_gaps() {
        let mut history = History::default();
        for index in 0..=RETAINED_RECORDS {
            history.ingest("brain", event(&index.to_string()), u64::from(index == 2));
        }
        assert_eq!(history.records.len(), RETAINED_RECORDS);
        let snapshot = history.snapshot(&payload::log::SnapshotRequest {
            participant_id: None,
            limit: u32::MAX,
            before_sequence: None,
        });
        assert_eq!(snapshot.records.len(), MAX_PAGE);
        assert_eq!(snapshot.cursor.sequence, 1_001);
        assert_eq!(snapshot.ingest_dropped, 1);
        assert_eq!(snapshot.records[0].sequence, 746);
        assert_eq!(snapshot.next_before_sequence, Some(746));
    }
}
