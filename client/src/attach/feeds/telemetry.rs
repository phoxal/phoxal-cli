//! The supervisor's retained runtime rollups, reconciled with its live feed.
//!
//! Same shape as the log feed and for the same reason: the daemon is the one
//! collector, so there is one cursor and one retention to reconcile against
//! (organization#978).

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use phoxal_cli_observation::{
    AttachmentEvent, RuntimeBufferKind, RuntimeDirection, RuntimeFeedStatus,
    RuntimePerformanceSample, RuntimeStepSample, RuntimeTopicSample, SourceStatus, StoreChanged,
    sanitize_terminal_text,
};
use phoxal_supervisor_api::{RuntimeTopic, TelemetryRecord, supervisor};

use super::FeedContext;
use crate::reconcile::{Cursor, ReconcileOutcome, Reconciler, RetryBackoff, Sequenced};

const SOURCE: &str = "telemetry";
const PAGE: u32 = 256;
const BUFFER: usize = 1_024;

/// Remote text reaches this client from another machine's supervisor, so it is
/// sanitized and bounded before it can ever reach a terminal.
const MAX_REMOTE_TEXT_CHARS: usize = 256;

pub(crate) async fn run(context: FeedContext) {
    super::until_cancelled(&context, SOURCE, feed(&context)).await;
}

async fn feed(context: &FeedContext) -> Result<()> {
    let attachment = &context.attachment;
    let subscriber = attachment.follow_telemetry().await?;
    let mut reconciler = Reconciler::new(BUFFER);
    let mut local_drops = subscriber.dropped();
    let mut backoff = RetryBackoff::new(Duration::from_millis(10), Duration::from_millis(250));
    let mut evictions = 0;

    'query: loop {
        reconciler.begin_query();
        let supervisor::telemetry::Snapshot::V0 {
            cursor,
            records,
            capacity_evictions,
            ..
        } = attachment.telemetry(None, PAGE, None).await?;
        evictions = capacity_evictions;
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
        apply(context, outcome, evictions).await?;
        context.health(SOURCE, SourceStatus::Live).await;
        backoff.reset();

        loop {
            let received = subscriber.recv().await?;
            let observed = subscriber.dropped();
            if observed != local_drops {
                local_drops = observed;
                let _ = reconciler.local_drop();
                while subscriber.try_recv().is_some() {}
                local_drops = subscriber.dropped();
                tokio::time::sleep(backoff.next_delay()).await;
                continue 'query;
            }
            let supervisor::telemetry::Follow::V0 { cursor, record } = received.body;
            let outcome = reconciler.follow(Follow {
                cursor: Cursor {
                    generation: cursor.generation.as_str().to_string(),
                    sequence: cursor.sequence,
                },
                record,
            });
            if matches!(outcome, ReconcileOutcome::Requery) {
                while subscriber.try_recv().is_some() {}
                local_drops = subscriber.dropped();
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
    let revision = match outcome {
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
    record: TelemetryRecord,
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

fn sample(record: TelemetryRecord) -> RuntimePerformanceSample {
    RuntimePerformanceSample {
        sequence: record.sequence,
        participant_id: bounded_remote_text(record.participant.as_str()),
        truncated: record.truncated,
        window_ns: record.window_ns,
        step: record.step.map(|step| RuntimeStepSample {
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
        topics: Arc::new(record.topics.into_iter().map(topic).collect()),
        overflow: record.overflow.map(topic),
    }
}

fn topic(value: RuntimeTopic) -> RuntimeTopicSample {
    RuntimeTopicSample {
        topic: if value.topic.as_str().is_empty() {
            "Other/unobserved topics".to_string()
        } else {
            bounded_remote_text(value.topic.as_str())
        },
        direction: match value.direction {
            phoxal_supervisor_api::RuntimeDirection::Publish => RuntimeDirection::Publish,
            phoxal_supervisor_api::RuntimeDirection::Subscribe => RuntimeDirection::Subscribe,
            phoxal_supervisor_api::RuntimeDirection::Mixed => RuntimeDirection::Mixed,
        },
        buffer_kind: match value.buffer_kind {
            phoxal_supervisor_api::RuntimeBufferKind::Outbound => RuntimeBufferKind::Outbound,
            phoxal_supervisor_api::RuntimeBufferKind::Latest => RuntimeBufferKind::Latest,
            phoxal_supervisor_api::RuntimeBufferKind::Subscriber => RuntimeBufferKind::Subscriber,
            phoxal_supervisor_api::RuntimeBufferKind::Mixed => RuntimeBufferKind::Mixed,
        },
        count: value.count,
        rate_hz: value.rate_hz,
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
