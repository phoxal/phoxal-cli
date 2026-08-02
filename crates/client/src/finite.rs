//! Finite and streaming observation ports for non-TUI command output.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use phoxal_api::v0_1 as api;
use phoxal_bus::{Bus, BusConfig};
use phoxal_bus::{ContractBody, DEFAULT_QUERY_TIMEOUT, Querier, Subscribe, Subscriber, Topic};
use phoxal_cli_core::identity::{ExecutionId, ProducerId};
use phoxal_cli_observation::sanitize_terminal_text;
use tokio::time::timeout;

use crate::reconcile::{Cursor, ReconcileOutcome, Reconciler, RetryBackoff, Sequenced};

#[derive(Debug, Clone)]
pub struct BusTarget {
    pub project_root: PathBuf,
    pub connect: String,
    pub namespace: Option<String>,
    pub robot_id: Option<String>,
    pub execution: ExecutionId,
}

async fn open_bus(target: &BusTarget, participant: &str) -> Result<Bus> {
    let (namespace, robot_id) = if let (Some(namespace), Some(robot_id)) =
        (&target.namespace, &target.robot_id)
    {
        (namespace.clone(), robot_id.clone())
    } else {
        let robot_path =
            phoxal_cli_core::project::resolver::discover_robot_yaml(&target.project_root)
                .with_context(|| {
                    format!(
                        "failed to find robot.yaml from {}; name the robot with --namespace and \
                         --robot-id to address a run from outside its project",
                        target.project_root.display()
                    )
                })?;
        let robot = phoxal_cli_core::project::resolver::load_robot(&robot_path)?;
        (
            target.namespace.clone().unwrap_or(robot.robot.namespace),
            target.robot_id.clone().unwrap_or(robot.robot.id),
        )
    };
    Ok(Bus::open(BusConfig {
        namespace,
        robot_id,
        execution: target.execution,
        participant: participant.to_string(),
        producer: ProducerId::mint(),
        connect_endpoints: vec![target.connect.clone()],
    })
    .await?)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogEvent {
    Record {
        participant_id: String,
        level: String,
        message: String,
        dropped: u32,
        truncated: u32,
    },
    Empty,
    NoMatch {
        participant: String,
    },
    ToolLoss {
        delta: u64,
        cumulative: u64,
    },
}

pub async fn stream_logs(
    target: &BusTarget,
    participant: Option<String>,
    follow: bool,
    mut emit: impl FnMut(LogEvent),
) -> Result<()> {
    let bus = open_bus(target, "phoxal-cli-logs").await?;
    let follow_topic = api::topic::client().supervisor().log().follow();
    let subscriber =
        Subscriber::<api::supervisor::log::Follow>::new(&bus, &follow_topic, 256).await?;
    let snapshot_topic = api::topic::client().supervisor().log().snapshot();
    let querier =
        Querier::<api::supervisor::log::SnapshotRequest, api::supervisor::log::Snapshot>::new(
            bus.clone(),
            &snapshot_topic,
            DEFAULT_QUERY_TIMEOUT,
        )?;
    let mut reconciler = Reconciler::new(512);
    let mut local_drops = subscriber.dropped();
    let mut tool_drops = 0_u64;
    let mut emitted_cursor: Option<Cursor> = None;
    let mut retry_backoff =
        RetryBackoff::new(Duration::from_millis(10), Duration::from_millis(250));

    'query: loop {
        reconciler.begin_query();
        let query = querier.query(api::supervisor::log::SnapshotRequest {});
        tokio::pin!(query);
        loop {
            tokio::select! {
                response = &mut query => {
                    let snapshot = response?;
                    disclose_tool_log_loss(snapshot.ingest_dropped, &mut tool_drops, &mut emit);
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
                            let emit_result = emit_new_records(
                                snapshot.into_iter().chain(replay),
                                participant.as_deref(),
                                &mut emitted_cursor,
                                &mut emit,
                            );
                            if !follow {
                                if emit_result.emitted == 0 {
                                    if emit_result.seen == 0 {
                                        emit(LogEvent::Empty);
                                    } else if let Some(participant) = participant.as_deref() {
                                        emit(LogEvent::NoMatch { participant: participant.to_string() });
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
                    disclose_tool_log_loss(follow_item.ingest_dropped, &mut tool_drops, &mut emit);
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
            disclose_tool_log_loss(follow_item.ingest_dropped, &mut tool_drops, &mut emit);
            match reconciler.follow(RetainedLog::from(follow_item)) {
                ReconcileOutcome::Append(record) => {
                    emit_new_records(
                        std::iter::once(record),
                        participant.as_deref(),
                        &mut emitted_cursor,
                        &mut emit,
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
    subscriber: &Subscriber<api::supervisor::log::Follow>,
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
    record: api::supervisor::log::Record,
}

impl From<api::supervisor::log::Follow> for RetainedLog {
    fn from(follow: api::supervisor::log::Follow) -> Self {
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

fn emit_new_records(
    records: impl IntoIterator<Item = RetainedLog>,
    participant: Option<&str>,
    emitted_cursor: &mut Option<Cursor>,
    emit: &mut impl FnMut(LogEvent),
) -> EmitResult {
    let mut result = EmitResult::default();
    for item in records {
        let already_emitted = emitted_cursor.as_ref().is_some_and(|cursor| {
            cursor.generation == item.cursor.generation && cursor.sequence >= item.cursor.sequence
        });
        if already_emitted {
            continue;
        }
        result.seen += 1;
        *emitted_cursor = Some(item.cursor);
        if participant.is_some_and(|participant| participant != item.record.participant_id) {
            continue;
        }
        emit(LogEvent::Record {
            participant_id: sanitize_terminal_text(&item.record.participant_id),
            level: format!("{:?}", item.record.level),
            message: sanitize_terminal_text(&item.record.message),
            dropped: item.record.dropped,
            truncated: item.record.truncated,
        });
        result.emitted += 1;
    }
    result
}

#[derive(Debug, Default, PartialEq, Eq)]
struct EmitResult {
    seen: usize,
    emitted: usize,
}

fn disclose_tool_log_loss(observed: u64, previous: &mut u64, emit: &mut impl FnMut(LogEvent)) {
    if observed > *previous {
        emit(LogEvent::ToolLoss {
            delta: observed.saturating_sub(*previous),
            cumulative: observed,
        });
        *previous = observed;
    }
}

#[derive(Debug, Clone, Copy)]
pub enum StatusQuery {
    Safety,
    Motion,
    Localization,
}

#[derive(Debug, Clone, PartialEq)]
pub enum StatusSnapshot {
    Safety {
        clear: bool,
        sequence: u64,
        expires_at: String,
        constraints: Vec<SafetyConstraint>,
    },
    Motion {
        selected_source: String,
        linear_x_mps: f32,
        angular_z_radps: f32,
        zero_reason: String,
        component_estop_blocked: bool,
        safety_constraints: usize,
    },
    Localization {
        x_m: f64,
        y_m: f64,
        yaw_rad: f64,
        confidence: f32,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct SafetyConstraint {
    pub reason: String,
    pub participant_id: String,
    pub stop: bool,
    pub max_linear_speed_mps: Option<f32>,
    pub observed_value: Option<f32>,
}

pub async fn query_status(target: &BusTarget, query: StatusQuery) -> Result<StatusSnapshot> {
    let (participant, topic_timeout) = match query {
        StatusQuery::Safety => ("phoxal-cli-safety-inspect", "safety"),
        StatusQuery::Motion => ("phoxal-cli-motion-inspect", "motion"),
        StatusQuery::Localization => ("phoxal-cli-localization-inspect", "localize"),
    };
    let bus = open_bus(target, participant).await?;
    let snapshot = match query {
        StatusQuery::Safety => {
            let topic = Topic::<Subscribe<api::safety::State>>::new_static(
                <api::safety::State as ContractBody>::TOPIC,
            );
            let state = timeout(
                Duration::from_secs(3),
                Subscriber::new(&bus, &topic, 32).await?.recv(),
            )
            .await
            .with_context(|| format!("{topic_timeout} did not publish domain state"))??
            .body;
            StatusSnapshot::Safety {
                clear: state.clear,
                sequence: state.motion.sequence,
                expires_at: state.motion.expires_at.to_string(),
                constraints: state
                    .motion
                    .constraints
                    .into_iter()
                    .map(|constraint| SafetyConstraint {
                        reason: sanitize_terminal_text(&format!("{:?}", constraint.reason)),
                        participant_id: sanitize_terminal_text(&constraint.source.participant_id),
                        stop: constraint.stop,
                        max_linear_speed_mps: constraint.max_linear_speed_mps,
                        observed_value: constraint.observed_value,
                    })
                    .collect(),
            }
        }
        StatusQuery::Motion => {
            let topic = Topic::<Subscribe<api::motion::State>>::new_static(
                <api::motion::State as ContractBody>::TOPIC,
            );
            let state = timeout(
                Duration::from_secs(3),
                Subscriber::new(&bus, &topic, 32).await?.recv(),
            )
            .await
            .with_context(|| format!("{topic_timeout} did not publish domain state"))??
            .body;
            StatusSnapshot::Motion {
                selected_source: format!("{:?}", state.selected_source),
                linear_x_mps: state.final_target.linear_x_mps,
                angular_z_radps: state.final_target.angular_z_radps,
                zero_reason: format!("{:?}", state.zero_reason),
                component_estop_blocked: state.component_estop_blocked,
                safety_constraints: state.active_safety_constraints.len(),
            }
        }
        StatusQuery::Localization => {
            let topic = Topic::<Subscribe<api::localize::LocalizationState>>::new_static(
                <api::localize::LocalizationState as ContractBody>::TOPIC,
            );
            let state = timeout(
                Duration::from_secs(3),
                Subscriber::new(&bus, &topic, 32).await?.recv(),
            )
            .await
            .with_context(|| format!("{topic_timeout} did not publish domain state"))??
            .body;
            StatusSnapshot::Localization {
                x_m: state.x_m,
                y_m: state.y_m,
                yaw_rad: state.yaw_rad,
                confidence: state.confidence,
            }
        }
    };
    bus.close().await?;
    Ok(snapshot)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn retained(sequence: u64, participant_id: &str) -> RetainedLog {
        RetainedLog {
            cursor: Cursor {
                generation: "g".to_string(),
                sequence,
            },
            record: api::supervisor::log::Record {
                sequence,
                participant_id: participant_id.to_string(),
                source_sequence: sequence,
                time: api::supervisor::log::Timestamp {
                    unix_seconds: 0,
                    nanos: 0,
                },
                level: api::supervisor::log::Level::Info,
                target: participant_id.to_string(),
                message: format!("record-{sequence}"),
                fields: BTreeMap::new(),
                dropped: 0,
                truncated: 0,
            },
        }
    }

    #[test]
    fn filtered_snapshot_distinguishes_events_from_matches() {
        let mut cursor = None;
        let mut events = Vec::new();
        let result = emit_new_records(
            [retained(1, "other"), retained(2, "wanted")],
            Some("wanted"),
            &mut cursor,
            &mut |event| events.push(event),
        );
        assert_eq!(result.seen, 2);
        assert_eq!(result.emitted, 1);
        assert_eq!(events.len(), 1);
    }

    #[test]
    fn finite_output_strips_terminal_control_sequences() {
        let mut cursor = None;
        let mut events = Vec::new();
        let mut item = retained(1, "participant\u{1b}]52;c;clipboard\u{7}");
        item.record.message = "before\u{1b}[2Jafter".to_string();

        emit_new_records([item], None, &mut cursor, &mut |event| events.push(event));

        assert_eq!(
            events,
            vec![LogEvent::Record {
                participant_id: "participant".to_string(),
                level: "Info".to_string(),
                message: "beforeafter".to_string(),
                dropped: 0,
                truncated: 0,
            }]
        );
        assert_eq!(
            sanitize_terminal_text("reason\u{1b}]0;title\u{1b}\\ participant"),
            "reason participant"
        );
    }
}
