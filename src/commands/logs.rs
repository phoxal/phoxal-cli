use anyhow::Result;
use clap::Args;
use phoxal::bus::{DEFAULT_QUERY_TIMEOUT, Querier, Subscriber};
use phoxal::raw::Bus;
use phoxal_api::v0_1 as api;
use std::time::Duration;

use crate::AppContext;
use crate::commands::bus_target::BusTargetArgs;
use phoxal_cli_core::session::reconcile::{
    Cursor, ReconcileOutcome, Reconciler, RetryBackoff, Sequenced,
};

#[derive(Debug, Args)]
pub struct Logs {
    #[arg(short = 'f', long, help = "Follow log events until interrupted.")]
    pub follow: bool,
    #[arg(
        value_name = "PARTICIPANT",
        help = "Participant id to show. Omit for all participants."
    )]
    pub participant: Option<String>,
    #[command(flatten)]
    pub target: BusTargetArgs,
}

impl Logs {
    pub async fn run(&self, app: &AppContext) -> Result<()> {
        let bus = self.target.open(app, "phoxal-cli-logs").await?;
        stream_logs(bus, self.participant.clone(), self.follow).await
    }
}

async fn stream_logs(bus: Bus, participant: Option<String>, follow: bool) -> Result<()> {
    let follow_topic = api::topic::client().tool().log().follow();
    let subscriber = Subscriber::<api::tool::log::Follow>::new(&bus, &follow_topic, 256).await?;
    let snapshot_topic = api::topic::client().tool().log().snapshot();
    let querier = Querier::<api::tool::log::SnapshotRequest, api::tool::log::Snapshot>::new(
        bus.clone(),
        &snapshot_topic,
        DEFAULT_QUERY_TIMEOUT,
    )?;
    let mut reconciler = Reconciler::new(512);
    let mut local_drops = subscriber.dropped();
    let mut tool_drops = 0_u64;
    let mut printed_cursor: Option<Cursor> = None;
    let mut retry_backoff =
        RetryBackoff::new(Duration::from_millis(10), Duration::from_millis(250));

    'query: loop {
        reconciler.begin_query();
        let query = querier.query(api::tool::log::SnapshotRequest {});
        tokio::pin!(query);
        loop {
            tokio::select! {
                response = &mut query => {
                    let snapshot = response?;
                    disclose_tool_log_loss(snapshot.ingest_dropped, &mut tool_drops);
                    let generation = snapshot.cursor.generation.clone();
                    let records = snapshot.records.into_iter().map(|record| RetainedLog {
                        cursor: Cursor { generation: generation.clone(), sequence: record.sequence },
                        record,
                    }).collect();
                    match reconciler.install(
                        Cursor { generation: snapshot.cursor.generation, sequence: snapshot.cursor.sequence },
                        records,
                    ) {
                        ReconcileOutcome::Installed { snapshot, replay } => {
                            let print_result = print_new_records(
                                snapshot.into_iter().chain(replay),
                                participant.as_deref(),
                                &mut printed_cursor,
                            );
                            if !follow {
                                if print_result.printed == 0 {
                                    if print_result.seen == 0 {
                                        eprintln!("no retained log events");
                                    } else if let Some(participant) = participant.as_deref() {
                                        eprintln!("no retained log events matched participant {participant}");
                                    }
                                }
                                bus.close().await?;
                                return Ok(());
                            }
                            retry_backoff.reset();
                            break;
                        }
                        ReconcileOutcome::Requery => {
                            prepare_log_requery(&subscriber, &mut local_drops, &mut retry_backoff).await;
                            continue 'query;
                        }
                        ReconcileOutcome::Buffered | ReconcileOutcome::Append(_) => {
                            unreachable!("snapshot installation has only installed/requery outcomes")
                        }
                    }
                }
                received = subscriber.recv() => {
                    let received = received?;
                    let observed = subscriber.dropped();
                    if observed != local_drops {
                        local_drops = observed;
                        let _ = reconciler.local_drop();
                        prepare_log_requery(&subscriber, &mut local_drops, &mut retry_backoff).await;
                        continue 'query;
                    }
                    let follow_item = received.body;
                    disclose_tool_log_loss(follow_item.ingest_dropped, &mut tool_drops);
                    if matches!(
                        reconciler.follow(RetainedLog::from(follow_item)),
                        ReconcileOutcome::Requery
                    ) {
                        prepare_log_requery(&subscriber, &mut local_drops, &mut retry_backoff).await;
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
                prepare_log_requery(&subscriber, &mut local_drops, &mut retry_backoff).await;
                continue 'query;
            }
            let follow_item = received.body;
            disclose_tool_log_loss(follow_item.ingest_dropped, &mut tool_drops);
            match reconciler.follow(RetainedLog::from(follow_item)) {
                ReconcileOutcome::Append(record) => {
                    print_new_records(
                        std::iter::once(record),
                        participant.as_deref(),
                        &mut printed_cursor,
                    );
                }
                ReconcileOutcome::Requery => {
                    prepare_log_requery(&subscriber, &mut local_drops, &mut retry_backoff).await;
                    continue 'query;
                }
                ReconcileOutcome::Buffered | ReconcileOutcome::Installed { .. } => {}
            }
        }
    }
}

async fn prepare_log_requery(
    subscriber: &Subscriber<api::tool::log::Follow>,
    local_drops: &mut u64,
    backoff: &mut RetryBackoff,
) {
    while subscriber.try_recv().is_some() {}
    *local_drops = subscriber.dropped();
    tokio::time::sleep(backoff.next_delay()).await;
}

#[derive(Debug)]
struct RetainedLog {
    cursor: Cursor,
    record: api::tool::log::Record,
}

impl From<api::tool::log::Follow> for RetainedLog {
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

impl Sequenced for RetainedLog {
    fn cursor(&self) -> Cursor {
        self.cursor.clone()
    }
}

fn print_new_records(
    records: impl IntoIterator<Item = RetainedLog>,
    participant: Option<&str>,
    printed_cursor: &mut Option<Cursor>,
) -> PrintResult {
    let mut result = PrintResult::default();
    for item in records {
        let already_printed = printed_cursor.as_ref().is_some_and(|cursor| {
            cursor.generation == item.cursor.generation && cursor.sequence >= item.cursor.sequence
        });
        if already_printed {
            continue;
        }
        result.seen += 1;
        *printed_cursor = Some(item.cursor);
        if participant.is_some_and(|participant| participant != item.record.participant_id) {
            continue;
        }
        print_record(&item.record);
        result.printed += 1;
    }
    result
}

#[derive(Debug, Default, PartialEq, Eq)]
struct PrintResult {
    seen: usize,
    printed: usize,
}

fn print_record(record: &api::tool::log::Record) {
    let mut message = format!("{:?}: {}", record.level, record.message);
    if record.dropped > 0 {
        message.push_str(&format!(" (producer dropped {})", record.dropped));
    }
    if record.truncated > 0 {
        message.push_str(&format!(" (truncated {})", record.truncated));
    }
    println!("[{}] {}", record.participant_id, message);
}

fn disclose_tool_log_loss(observed: u64, previous: &mut u64) {
    if observed > *previous {
        eprintln!(
            "tool-log lost {} event(s) before retention ({} cumulative)",
            observed.saturating_sub(*previous),
            observed
        );
        *previous = observed;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn filtered_snapshot_distinguishes_events_from_matches() {
        let item = RetainedLog {
            cursor: Cursor {
                generation: "g".to_string(),
                sequence: 1,
            },
            record: api::tool::log::Record {
                sequence: 1,
                participant_id: "drive".to_string(),
                source_sequence: 1,
                time: api::tool::log::Timestamp {
                    unix_seconds: 0,
                    nanos: 0,
                },
                level: api::tool::log::Level::Info,
                target: "drive".to_string(),
                message: "ready".to_string(),
                fields: BTreeMap::new(),
                dropped: 0,
                truncated: 0,
            },
        };
        let mut cursor = None;
        assert_eq!(
            print_new_records([item], Some("mission"), &mut cursor),
            PrintResult {
                seen: 1,
                printed: 0
            }
        );
    }
}
