use anyhow::{Context, Result};
use clap::Args;
use phoxal::bus::{DEFAULT_QUERY_TIMEOUT, Querier, Subscriber};
use phoxal::raw::{Bus, BusConfig};
use phoxal_api::v1 as api;
use std::path::Path;

use crate::AppContext;
use phoxal_cli_core::project::launch_plan::DEFAULT_ROUTER_CONNECT;
use phoxal_cli_core::project::resolver::{discover_robot_yaml, load_robot_with_extras};
use phoxal_cli_core::session::reconcile::{Cursor, ReconcileOutcome, Reconciler, Sequenced};

#[derive(Debug, Args)]
pub struct Logs {
    #[arg(short = 'f', long, help = "Follow log events until interrupted.")]
    pub follow: bool,
    #[arg(
        value_name = "PARTICIPANT",
        help = "Participant id to show. Omit for all participants."
    )]
    pub participant: Option<String>,
    #[arg(
        long,
        value_name = "ENDPOINT",
        default_value = DEFAULT_ROUTER_CONNECT,
        help = "Router endpoint to connect to."
    )]
    pub connect: String,
    #[arg(
        long,
        value_name = "NAMESPACE",
        help = "Robot namespace. Defaults to robot.yaml."
    )]
    pub namespace: Option<String>,
    #[arg(
        long = "robot-id",
        value_name = "ID",
        help = "Robot id. Defaults to robot.yaml."
    )]
    pub robot_id: Option<String>,
}

impl Logs {
    pub async fn run(&self, app: &AppContext) -> Result<()> {
        let (namespace, robot_id) = resolve_identity(
            app.project.root(),
            self.namespace.clone(),
            self.robot_id.clone(),
        )?;
        stream_logs(
            namespace,
            robot_id,
            self.connect.clone(),
            self.participant.clone(),
            self.follow,
        )
        .await
    }
}

fn resolve_identity(
    project_start: &Path,
    namespace: Option<String>,
    robot_id: Option<String>,
) -> Result<(String, String)> {
    match (namespace, robot_id) {
        (Some(namespace), Some(robot_id)) => Ok((namespace, robot_id)),
        (namespace, robot_id) => {
            let robot_path = discover_robot_yaml(project_start).with_context(|| {
                format!("failed to find robot.yaml from {}", project_start.display())
            })?;
            let loaded = load_robot_with_extras(&robot_path)?;
            Ok((
                namespace.unwrap_or(loaded.robot.robot.namespace),
                robot_id.unwrap_or(loaded.robot.robot.id),
            ))
        }
    }
}

async fn stream_logs(
    namespace: String,
    robot_id: String,
    connect: String,
    participant: Option<String>,
    follow: bool,
) -> Result<()> {
    let bus = Bus::open(BusConfig {
        namespace,
        robot_id,
        participant: "phoxal-cli-logs".to_string(),
        incarnation: 0,
        connect_endpoints: vec![connect],
    })
    .await?;
    let follow_topic = api::topic::new().tool().log().follow();
    let subscriber = Subscriber::<api::tool::log::Follow>::new(&bus, &follow_topic, 256).await?;
    let snapshot_topic = api::topic::new().tool().log().snapshot();
    let querier = Querier::<api::tool::log::SnapshotRequest, api::tool::log::Snapshot>::new(
        bus.clone(),
        &snapshot_topic,
        DEFAULT_QUERY_TIMEOUT,
    )?;
    let mut reconciler = Reconciler::new(512);
    let mut local_drops = subscriber.dropped();
    let mut tool_drops = 0_u64;
    let mut printed_cursor: Option<Cursor> = None;

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
                            let printed = print_new_records(
                                snapshot.into_iter().chain(replay),
                                participant.as_deref(),
                                &mut printed_cursor,
                            );
                            if !follow {
                                if printed == 0 {
                                    eprintln!("no retained log events");
                                }
                                bus.close().await?;
                                return Ok(());
                            }
                            break;
                        }
                        ReconcileOutcome::Requery => continue 'query,
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
                        continue 'query;
                    }
                    let follow = received.body;
                    disclose_tool_log_loss(follow.ingest_dropped, &mut tool_drops);
                    if matches!(
                        reconciler.follow(RetainedLog::from(follow)),
                        ReconcileOutcome::Requery
                    ) {
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
                continue 'query;
            }
            let follow = received.body;
            disclose_tool_log_loss(follow.ingest_dropped, &mut tool_drops);
            match reconciler.follow(RetainedLog::from(follow)) {
                ReconcileOutcome::Append(record) => {
                    print_new_records(
                        std::iter::once(record),
                        participant.as_deref(),
                        &mut printed_cursor,
                    );
                }
                ReconcileOutcome::Requery => continue 'query,
                ReconcileOutcome::Buffered | ReconcileOutcome::Installed { .. } => {}
            }
        }
    }
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
) -> usize {
    let mut printed = 0;
    for item in records {
        let already_printed = printed_cursor.as_ref().is_some_and(|cursor| {
            cursor.generation == item.cursor.generation && cursor.sequence >= item.cursor.sequence
        });
        if already_printed {
            continue;
        }
        *printed_cursor = Some(item.cursor);
        if participant.is_some_and(|participant| participant != item.record.participant_id) {
            continue;
        }
        print_record(&item.record);
        printed += 1;
    }
    printed
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
