//! The supervisor's retained participant logs, reconciled with its live feed.
//!
//! The endpoint is the supervisor's own (`supervisor/logs/...`), not a
//! robot-domain tool's: the daemon is the collector now, so there is one
//! retention and one cursor rather than a tool's history duplicated beside the
//! supervisor's (organization#978).

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use phoxal_cli_observation::{
    AttachmentEvent, LogRow, LogSeverity, LogSource, SourceStatus, StoreChanged, bounded_log_text,
};
use phoxal_supervisor_api::{LogLevel, LogRecord, WallTime, supervisor};

use super::FeedContext;
use crate::reconcile::{Cursor, ReconcileOutcome, Reconciler, RetryBackoff, Sequenced};

const SOURCE: &str = "logs";

/// How many records one backward page asks for. The supervisor bounds the page
/// itself; this is the client's own appetite.
const PAGE: u32 = 512;

/// Buffered follow records while a query is in flight.
const BUFFER: usize = 1_024;

pub(crate) async fn run(context: FeedContext) {
    super::until_cancelled(&context, SOURCE, feed(&context)).await;
}

async fn feed(context: &FeedContext) -> Result<()> {
    let attachment = &context.attachment;
    let subscriber = attachment.follow_logs().await?;
    let mut reconciler = Reconciler::new(BUFFER);
    let mut local_drops = subscriber.dropped();
    let mut backoff = RetryBackoff::new(Duration::from_millis(10), Duration::from_millis(250));

    'query: loop {
        reconciler.begin_query();
        let page = attachment.logs(None, PAGE, None).await?;
        let supervisor::logs::Snapshot::V0 {
            cursor, records, ..
        } = page;
        let anchor = Cursor {
            generation: cursor.generation.as_str().to_string(),
            sequence: cursor.sequence,
        };
        let installed = records
            .into_iter()
            .map(|record| Follow {
                cursor: Cursor {
                    generation: anchor.generation.clone(),
                    sequence: record.sequence,
                },
                record,
            })
            .collect();
        let outcome = reconciler.install(anchor, installed);
        apply(context, outcome).await?;
        context.health(SOURCE, SourceStatus::Live).await;
        backoff.reset();

        loop {
            let received = subscriber.recv().await?;
            let observed = subscriber.dropped();
            if observed != local_drops {
                // A record this client's own inbound ring dropped is a hole in
                // the sequence, and a hole means the installed page can no
                // longer be continued: re-query rather than splice.
                local_drops = observed;
                let _ = reconciler.local_drop();
                requery(&subscriber, &mut local_drops, &mut backoff).await;
                continue 'query;
            }
            let supervisor::logs::Follow::V0 { cursor, record, .. } = received.body;
            let outcome = reconciler.follow(Follow {
                cursor: Cursor {
                    generation: cursor.generation.as_str().to_string(),
                    sequence: cursor.sequence,
                },
                record,
            });
            if matches!(outcome, ReconcileOutcome::Requery) {
                requery(&subscriber, &mut local_drops, &mut backoff).await;
                continue 'query;
            }
            apply(context, outcome).await?;
        }
    }
}

async fn apply(context: &FeedContext, outcome: ReconcileOutcome<Follow>) -> Result<()> {
    let revision = match outcome {
        ReconcileOutcome::Installed { snapshot, replay } => {
            context.stores.logs.write().await.install_snapshot(
                context.epoch,
                snapshot.into_iter().chain(replay).map(|item| row(item.record)),
            )
        }
        ReconcileOutcome::Append(item) => context
            .stores
            .logs
            .write()
            .await
            .record(context.epoch, row(item.record)),
        ReconcileOutcome::Buffered | ReconcileOutcome::Requery => return Ok(()),
    };
    if let Some(revision) = revision {
        context
            .events
            .send(AttachmentEvent::LogsChanged(StoreChanged {
                epoch: context.epoch,
                revision,
            }))
            .await?;
    }
    Ok(())
}

async fn requery<T>(
    subscriber: &phoxal_bus::Subscriber<T>,
    local_drops: &mut u64,
    backoff: &mut RetryBackoff,
) where
    T: phoxal_bus::ContractBody,
{
    while subscriber.try_recv().is_some() {}
    *local_drops = subscriber.dropped();
    tokio::time::sleep(backoff.next_delay()).await;
}

#[derive(Debug, Clone)]
struct Follow {
    cursor: Cursor,
    record: LogRecord,
}

impl Sequenced for Follow {
    fn cursor(&self) -> Cursor {
        self.cursor.clone()
    }
}

fn row(record: LogRecord) -> LogRow {
    let mut text = format!("{:?}: {}", record.level, record.message);
    if record.dropped > 0 {
        text.push_str(&format!(" (producer dropped {})", record.dropped));
    }
    if record.truncated > 0 {
        text.push_str(&format!(" (truncated {})", record.truncated));
    }
    LogRow {
        participant: record.participant.as_str().to_string(),
        source: LogSource::Bus,
        severity: match record.level {
            LogLevel::Error => LogSeverity::Error,
            LogLevel::Warn => LogSeverity::Warn,
            LogLevel::Info => LogSeverity::Info,
            LogLevel::Debug => LogSeverity::Debug,
            LogLevel::Trace => LogSeverity::Trace,
        },
        text: bounded_log_text(&text),
        event_time: wall_time(record.time),
    }
}

/// The record's own wall clock, which may be another machine's. It is rendered,
/// never compared against this client's monotonic clock.
fn wall_time(time: WallTime) -> SystemTime {
    let nanos = Duration::from_nanos(u64::from(time.nanos.min(999_999_999)));
    let seconds = if time.unix_seconds >= 0 {
        UNIX_EPOCH.checked_add(Duration::from_secs(time.unix_seconds.unsigned_abs()))
    } else {
        UNIX_EPOCH.checked_sub(Duration::from_secs(time.unix_seconds.unsigned_abs()))
    };
    seconds
        .and_then(|value| value.checked_add(nanos))
        .unwrap_or(UNIX_EPOCH)
}
