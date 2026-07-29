use std::convert::Infallible;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Result, anyhow};
use phoxal::bus::{DEFAULT_QUERY_TIMEOUT, Querier, Subscriber};
use phoxal::raw::Bus;
use phoxal_api::v0_1 as api;
use phoxal_cli_observation::{
    AttachmentEpoch, AttachmentEvent, LogRow, LogScope, LogSeverity, LogSource, StoreChanged,
    bounded_log_text,
};
use tokio::sync::{RwLock, mpsc};

use crate::reconcile::{Cursor, ReconcileOutcome, Reconciler, RetryBackoff, Sequenced};
use crate::state::logs::LogStore;

pub(crate) async fn run(
    bus: Bus,
    scope: LogScope,
    epoch: AttachmentEpoch,
    store: Arc<RwLock<LogStore>>,
    events: mpsc::Sender<AttachmentEvent>,
    telemetry: super::TelemetryBackend,
) -> Result<Infallible> {
    let follow_topic = api::topic::client().tool().log().follow();
    let subscriber = Subscriber::<api::tool::log::Follow>::new(&bus, &follow_topic, 256).await?;
    let snapshot_topic = api::topic::client().tool().log().snapshot();
    let querier = Querier::<api::tool::log::SnapshotRequest, api::tool::log::Snapshot>::new(
        bus,
        &snapshot_topic,
        DEFAULT_QUERY_TIMEOUT,
    )?;
    let mut reconciler = Reconciler::new(512);
    let mut local_drops = subscriber.dropped();
    let mut retry_backoff =
        RetryBackoff::new(Duration::from_millis(10), Duration::from_millis(250));

    'query: loop {
        reconciler.begin_query();
        let query = querier.query(api::tool::log::SnapshotRequest {});
        tokio::pin!(query);
        loop {
            tokio::select! {
                response = &mut query => {
                    let snapshot = response.map_err(|error| anyhow!("tool-log snapshot query failed: {error}"))?;
                    let generation = snapshot.cursor.generation.clone();
                    let records = snapshot.records.into_iter().map(|record| RetainedLogFollow {
                        cursor: Cursor { generation: generation.clone(), sequence: record.sequence },
                        record,
                    }).collect();
                    let outcome = reconciler.install(
                        Cursor { generation: snapshot.cursor.generation, sequence: snapshot.cursor.sequence },
                        records,
                    );
                    apply_outcome(
                        &store,
                        &events,
                        &telemetry,
                        epoch,
                        &scope,
                        outcome,
                    )
                    .await?;
                    retry_backoff.reset();
                    break;
                }
                received = subscriber.recv() => {
                    let received = received?;
                    let observed = subscriber.dropped();
                    if observed != local_drops {
                        local_drops = observed;
                        let _ = reconciler.local_drop();
                        prepare_requery(&subscriber, &mut local_drops, &mut retry_backoff).await;
                        continue 'query;
                    }
                    if matches!(reconciler.follow(RetainedLogFollow::from(received.body)), ReconcileOutcome::Requery) {
                        prepare_requery(&subscriber, &mut local_drops, &mut retry_backoff).await;
                        continue 'query;
                    }
                }
            }
        }
        loop {
            let received = subscriber.recv().await?;
            let observed = subscriber.dropped();
            if observed != local_drops {
                local_drops = observed;
                let _ = reconciler.local_drop();
                prepare_requery(&subscriber, &mut local_drops, &mut retry_backoff).await;
                continue 'query;
            }
            let outcome = reconciler.follow(RetainedLogFollow::from(received.body));
            if matches!(outcome, ReconcileOutcome::Requery) {
                prepare_requery(&subscriber, &mut local_drops, &mut retry_backoff).await;
                continue 'query;
            }
            apply_outcome(&store, &events, &telemetry, epoch, &scope, outcome).await?;
        }
    }
}

async fn apply_outcome(
    store: &Arc<RwLock<LogStore>>,
    events: &mpsc::Sender<AttachmentEvent>,
    telemetry: &super::TelemetryBackend,
    epoch: AttachmentEpoch,
    scope: &LogScope,
    outcome: ReconcileOutcome<RetainedLogFollow>,
) -> Result<()> {
    let revision = match outcome {
        ReconcileOutcome::Installed { snapshot, replay } => store.write().await.install_snapshot(
            epoch,
            scope,
            snapshot
                .into_iter()
                .chain(replay)
                .map(|item| retained_log_row(scope, item.record)),
        ),
        ReconcileOutcome::Append(item) => store
            .write()
            .await
            .record(epoch, retained_log_row(scope, item.record)),
        ReconcileOutcome::Buffered => return Ok(()),
        ReconcileOutcome::Requery => return Ok(()),
    };
    telemetry.record_health(
        format!("logs:{}/{}", scope.namespace, scope.robot_id),
        phoxal_cli_observation::SourceStatus::Live,
    );
    if let Some(revision) = revision {
        events
            .send(AttachmentEvent::LogsChanged(StoreChanged {
                epoch,
                revision,
            }))
            .await?;
    }
    Ok(())
}

async fn prepare_requery(
    subscriber: &Subscriber<api::tool::log::Follow>,
    local_drops: &mut u64,
    backoff: &mut RetryBackoff,
) {
    while subscriber.try_recv().is_some() {}
    *local_drops = subscriber.dropped();
    tokio::time::sleep(backoff.next_delay()).await;
}

#[derive(Debug, Clone)]
struct RetainedLogFollow {
    cursor: Cursor,
    record: api::tool::log::Record,
}

impl From<api::tool::log::Follow> for RetainedLogFollow {
    fn from(follow: api::tool::log::Follow) -> Self {
        Self {
            cursor: Cursor {
                generation: follow.cursor.generation,
                sequence: follow.cursor.sequence,
            },
            record: follow.record,
        }
    }
}

impl Sequenced for RetainedLogFollow {
    fn cursor(&self) -> Cursor {
        self.cursor.clone()
    }
}

fn retained_log_row(scope: &LogScope, record: api::tool::log::Record) -> LogRow {
    let mut text = format!("{:?}: {}", record.level, record.message);
    if record.dropped > 0 {
        text.push_str(&format!(" (producer dropped {})", record.dropped));
    }
    if record.truncated > 0 {
        text.push_str(&format!(" (truncated {})", record.truncated));
    }
    LogRow {
        participant: format!(
            "{}/{}::{}",
            scope.namespace,
            scope.robot_id,
            super::bounded_remote_text(&record.participant_id)
        ),
        source: LogSource::Bus,
        severity: match record.level {
            api::tool::log::Level::Error => LogSeverity::Error,
            api::tool::log::Level::Warn => LogSeverity::Warn,
            api::tool::log::Level::Info => LogSeverity::Info,
            api::tool::log::Level::Debug => LogSeverity::Debug,
            api::tool::log::Level::Trace => LogSeverity::Trace,
        },
        text: bounded_log_text(&text),
        event_time: retained_log_time(&record.time),
        scope: Some(scope.clone()),
    }
}

fn retained_log_time(time: &api::tool::log::Timestamp) -> SystemTime {
    let nanos = Duration::from_nanos(u64::from(time.nanos.min(999_999_999)));
    let seconds = if time.unix_seconds >= 0 {
        UNIX_EPOCH.checked_add(Duration::from_secs(time.unix_seconds as u64))
    } else {
        UNIX_EPOCH.checked_sub(Duration::from_secs(time.unix_seconds.unsigned_abs()))
    };
    seconds
        .and_then(|value| value.checked_add(nanos))
        .unwrap_or(UNIX_EPOCH)
}
