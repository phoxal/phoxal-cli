//! The supervisor's retained participant logs, reconciled with its live feed.
//!
//! The endpoints are the supervisor's own retained view, not the live
//! `runtime/logs` stream every participant publishes to: the daemon is the
//! collector, so there is one retention and one cursor rather than a second
//! history kept beside the supervisor's.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use phoxal_api::runtime::telemetry::Cursor;
use phoxal_api::supervisor::{self, logs};
use phoxal_cli_observation::{
    AttachmentEvent, LogRow, LogSeverity, LogSource, ObservationSource, SourceStatus, StoreChanged,
    bounded_log_text,
};

use super::FeedContext;
use crate::reconcile::{ReconcileOutcome, Reconciler, RetryBackoff, Sequenced};

const SOURCE: ObservationSource = ObservationSource::Logs;

/// Buffered follow records while a query is in flight.
const BUFFER: usize = 1_024;
const PAGE: u32 = 256;

pub(crate) async fn run(context: FeedContext) {
    super::until_cancelled(&context, SOURCE, feed(&context)).await;
}

async fn feed(context: &FeedContext) -> Result<()> {
    let attachment = &context.attachment;
    let subscriber = attachment.follow_logs().await?;
    let mut reconciler = Reconciler::new(BUFFER);
    let mut backoff = RetryBackoff::new(Duration::from_millis(10), Duration::from_millis(250));

    'query: loop {
        reconciler.begin_query();
        let page = attachment.logs(None, PAGE, None).await?;
        let logs::Snapshot {
            cursor, records, ..
        } = page;
        let anchor = cursor;
        let installed = records
            .into_iter()
            .map(|record| Follow {
                cursor: Cursor {
                    sequence: record.sequence,
                },
                record,
            })
            .collect();
        let outcome = reconciler.install(anchor, installed);
        if matches!(outcome, ReconcileOutcome::Requery) {
            requery(&subscriber, &mut backoff).await;
            continue 'query;
        }
        apply(context, outcome).await?;
        context.health(SOURCE, SourceStatus::Live).await;
        backoff.reset();

        loop {
            let received = subscriber.recv().await?;
            let logs::Follow { cursor, record, .. } = received.body;
            let outcome = reconciler.follow(Follow {
                cursor: Cursor {
                    sequence: cursor.sequence,
                },
                record,
            });
            if matches!(outcome, ReconcileOutcome::Requery) {
                requery(&subscriber, &mut backoff).await;
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
                snapshot
                    .into_iter()
                    .chain(replay)
                    .map(|item| row(item.record)),
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

async fn requery(
    subscriber: &phoxal_bus::StreamReceiver<supervisor::endpoint::logs::FollowEndpoint>,
    backoff: &mut RetryBackoff,
) {
    while subscriber.try_recv().is_ok_and(|item| item.is_some()) {}
    tokio::time::sleep(backoff.next_delay()).await;
}

#[derive(Debug, Clone)]
struct Follow {
    cursor: Cursor,
    record: logs::Record,
}

impl Sequenced for Follow {
    fn cursor(&self) -> Cursor {
        self.cursor.clone()
    }
}

fn row(record: logs::Record) -> LogRow {
    let mut text = format!("{:?}: {}", record.level, record.message);
    if record.dropped > 0 {
        text.push_str(&format!(" (producer dropped {})", record.dropped));
    }
    if record.truncated > 0 {
        text.push_str(&format!(" (truncated {})", record.truncated));
    }
    LogRow {
        participant: record.participant_id,
        source: LogSource::Bus,
        severity: match record.level {
            logs::Level::Error => LogSeverity::Error,
            logs::Level::Warn => LogSeverity::Warn,
            logs::Level::Info => LogSeverity::Info,
            logs::Level::Debug => LogSeverity::Debug,
            logs::Level::Trace => LogSeverity::Trace,
        },
        text: bounded_log_text(&text),
        event_time: wall_time(record.time),
    }
}

/// The record's own wall clock, which may be another machine's. It is rendered,
/// never compared against this client's monotonic clock.
fn wall_time(time: logs::Timestamp) -> SystemTime {
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
