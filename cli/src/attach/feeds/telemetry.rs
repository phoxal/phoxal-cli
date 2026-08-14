//! The supervisor's retained runtime rollups, reconciled with its live feed.
//!
//! Same shape as the log feed and for the same reason: the supervisor is the one
//! collector, so there is one cursor and one retention to reconcile against.

use std::time::Duration;

use anyhow::Result;
use phoxal_cli_observation::{
    AttachmentEvent, ObservationSource, RuntimeFeedStatus, RuntimePerformanceSample, SourceStatus,
    StoreChanged, sanitize_terminal_text,
};
use phoxal_client::runtime::telemetry::{Cursor, Topic};
use phoxal_client::supervisor::telemetry;

use super::FeedContext;
use crate::reconcile::{ReconcileOutcome, Reconciler, RetryBackoff, Sequenced};

const SOURCE: ObservationSource = ObservationSource::Telemetry;
const PAGE: u32 = 256;
const BUFFER: usize = 1_024;

/// Remote text reaches this client from another machine's supervisor, so it is
/// sanitized and bounded before it can ever reach a terminal.
const MAX_REMOTE_TEXT_CHARS: usize = 256;

pub(crate) async fn run(context: FeedContext) {
    super::until_cancelled(&context, SOURCE, feed(&context)).await;
}

async fn feed(context: &FeedContext) -> Result<()> {
    let client = &context.client;
    let subscriber = client.follow_telemetry().await?;
    let mut reconciler = Reconciler::new(BUFFER);
    let mut backoff = RetryBackoff::new(Duration::from_millis(10), Duration::from_millis(250));

    'query: loop {
        reconciler.begin_query();
        let telemetry::Snapshot {
            cursor,
            records,
            capacity_evictions,
            ..
        } = client.telemetry(None, PAGE, None).await?;
        let evictions = capacity_evictions;
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
            while subscriber.try_recv().is_ok_and(|item| item.is_some()) {}
            tokio::time::sleep(backoff.next_delay()).await;
            continue 'query;
        }
        apply(context, outcome, evictions).await?;
        context.health(SOURCE, SourceStatus::Live).await;
        backoff.reset();

        loop {
            let received = subscriber.recv().await?;
            let telemetry::Follow { cursor, record } = received.body;
            let outcome = reconciler.follow(Follow {
                cursor: Cursor {
                    sequence: cursor.sequence,
                },
                record,
            });
            if matches!(outcome, ReconcileOutcome::Requery) {
                while subscriber.try_recv().is_ok_and(|item| item.is_some()) {}
                tokio::time::sleep(backoff.next_delay()).await;
                continue 'query;
            }
            apply(context, outcome, evictions).await?;
        }
    }
}

async fn apply(
    context: &FeedContext,
    outcome: ReconcileOutcome<Follow>,
    capacity_evictions: u64,
) -> Result<()> {
    let status = RuntimeFeedStatus { capacity_evictions };
    let revision =
        match outcome {
            ReconcileOutcome::Installed { snapshot, replay } => {
                context.stores.runtimes.write().await.install_snapshot(
                    context.epoch,
                    snapshot
                        .into_iter()
                        .chain(replay)
                        .map(|item| sample(item.record)),
                    status,
                )
            }
            ReconcileOutcome::Append(item) => context.stores.runtimes.write().await.record(
                context.epoch,
                sample(item.record),
                status,
            ),
            ReconcileOutcome::Buffered | ReconcileOutcome::Requery => return Ok(()),
        };
    if let Some(revision) = revision {
        context
            .events
            .send(AttachmentEvent::RuntimesChanged(StoreChanged {
                epoch: context.epoch,
                revision,
            }))
            .await?;
    }
    Ok(())
}

#[derive(Debug, Clone)]
struct Follow {
    cursor: Cursor,
    record: telemetry::Record,
}

impl Sequenced for Follow {
    fn cursor(&self) -> Cursor {
        self.cursor.clone()
    }
}

fn bounded_remote_text(text: &str) -> String {
    sanitize_terminal_text(text)
        .chars()
        .take(MAX_REMOTE_TEXT_CHARS)
        .collect()
}

fn sample(record: telemetry::Record) -> RuntimePerformanceSample {
    let mut record = record;
    record.participant_id = bounded_remote_text(&record.participant_id);
    record.topics.iter_mut().for_each(sanitize_topic);
    if let Some(overflow) = &mut record.overflow {
        sanitize_topic(overflow);
    }
    RuntimePerformanceSample { record }
}

fn sanitize_topic(value: &mut Topic) {
    value.topic = if value.topic.is_empty() {
        "Other/unobserved topics".to_string()
    } else {
        bounded_remote_text(&value.topic)
    };
}
