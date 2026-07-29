//! Disposable log and simulation-clock observation adapters.

use anyhow::Result;
use anyhow::anyhow;
use phoxal::bus::{DEFAULT_QUERY_TIMEOUT, Querier, Subscribe, Subscriber, Topic};
use phoxal::raw::{Bus, BusConfig, ParticipantLivelinessEvent, ParticipantLivelinessStatus};
use phoxal_api::v0_1 as api;
use phoxal_cli_core::identity::{ExecutionId, ProducerId};
use phoxal_cli_core::session::reconcile::{
    Cursor, ReconcileOutcome, Reconciler, RetryBackoff, Sequenced,
};
use phoxal_cli_core::session::stores::telemetry::RobotScope;
use phoxal_cli_core::session::telemetry::{ClockObservation, ClockSample};
use phoxal_cli_core::session::{
    BoardSnapshot, LogScope, LogSeverity, LogSource, ParticipantState, ParticipantStatus,
    ProcessScope, RoutedLogLine, RoutedLogUpdate, bounded_log_text,
};
use phoxal_cli_protocol::SupervisorSnapshotV0;
use std::collections::BTreeSet;
use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

pub(crate) const BUS_TELEMETRY_PARTICIPANT: &str = "phoxal-cli-tool-bus-consumer";
pub(crate) const CLOCK_OBSERVER_PARTICIPANT: &str = "phoxal-cli-clock-observer";
pub(crate) const CONTROL_STATE_PARTICIPANT: &str = "phoxal-cli-control-state";
pub(crate) const DEVICE_TELEMETRY_PARTICIPANT: &str = "phoxal-cli-tool-device-consumer";
pub(crate) const JOYPAD_TELEMETRY_PARTICIPANT: &str = "phoxal-cli-telemetry-joypad";
pub(crate) const LOG_OBSERVER_PARTICIPANT: &str = "phoxal-cli-log-observer";
pub(crate) const PRESENCE_OBSERVER_PARTICIPANT: &str = "phoxal-cli-presence-observer";
pub(crate) const RUNTIME_TELEMETRY_PARTICIPANT: &str = "phoxal-cli-tool-telemetry-consumer";

fn is_disposable_observer(participant: &str) -> bool {
    matches!(
        participant,
        BUS_TELEMETRY_PARTICIPANT
            | CLOCK_OBSERVER_PARTICIPANT
            | CONTROL_STATE_PARTICIPANT
            | DEVICE_TELEMETRY_PARTICIPANT
            | JOYPAD_TELEMETRY_PARTICIPANT
            | LOG_OBSERVER_PARTICIPANT
            | PRESENCE_OBSERVER_PARTICIPANT
            | RUNTIME_TELEMETRY_PARTICIPANT
    )
}

/// Transitional disposable projection used by the current attachment UI.
///
/// WS4 moves this state into `phoxal-cli-client`; keeping it here during WS3
/// prevents resident authority from retaining client observations or UI
/// channels.
#[derive(Debug, Clone, Default)]
pub(crate) struct ClientProjection {
    board: Arc<Mutex<BoardSnapshot>>,
    graph_generation: Arc<Mutex<Option<u64>>>,
    log_sink: Arc<Mutex<Option<mpsc::Sender<RoutedLogUpdate>>>>,
    unknown_bus_ids: Arc<Mutex<BTreeSet<String>>>,
}

impl ClientProjection {
    pub(crate) fn replace_supervisor(&self, snapshot: &SupervisorSnapshotV0) {
        let same_graph = self
            .graph_generation
            .lock()
            .expect("graph generation mutex poisoned")
            .replace(snapshot.graph_generation)
            == Some(snapshot.graph_generation);
        let mut board = self.board.lock().expect("client projection mutex poisoned");
        let previous = std::mem::take(&mut board.participants);
        for (key, entry) in &snapshot.processes {
            let id = key.to_string();
            let mut status = if same_graph {
                previous.get(&id).cloned().unwrap_or_else(|| {
                    ParticipantStatus::new(
                        &id,
                        entry.descriptor.kind,
                        participant_state(entry.status.actual),
                    )
                })
            } else {
                ParticipantStatus::new(
                    &id,
                    entry.descriptor.kind,
                    participant_state(entry.status.actual),
                )
            };
            status.kind = entry.descriptor.kind;
            status.state = participant_state(entry.status.actual);
            status.pid = entry.status.pid;
            status.restart_count = entry.status.restart_count_in_generation;
            status.note = entry
                .status
                .last_failure
                .as_ref()
                .map(|failure| failure.detail.as_str().to_string())
                .or(status.note);
            if let ProcessScope::Robot(robot) = &key.scope {
                status.scope = Some(RobotScope {
                    namespace: robot.namespace.clone(),
                    robot_id: robot.robot_id.clone(),
                });
            }
            board.participants.insert(id, status);
        }
    }

    pub(crate) fn snapshot(&self) -> BoardSnapshot {
        self.board
            .lock()
            .expect("client projection mutex poisoned")
            .clone()
    }

    pub(crate) fn set_log_sink(&self, sender: mpsc::Sender<RoutedLogUpdate>) {
        *self.log_sink.lock().expect("log sink mutex poisoned") = Some(sender);
    }

    pub(crate) fn record_presence(
        &self,
        namespace: &str,
        robot_id: &str,
        participant: &str,
        present: bool,
    ) {
        let robot_key = format!("{namespace}/{robot_id}::{participant}");
        let mut board = self.board.lock().expect("client projection mutex poisoned");
        let id = if board.participants.contains_key(&robot_key) {
            robot_key.clone()
        } else {
            participant.to_string()
        };
        if let Some(status) = board.participants.get_mut(&id) {
            status.present = Some(present);
        } else {
            drop(board);
            self.disclose_unknown_bus_id(&robot_key, "liveliness");
        }
    }

    fn route_log_line(&self, mut line: RoutedLogLine) -> bool {
        line.text = bounded_log_text(&line.text);
        let id = self
            .display_id_for_bus_line(&line)
            .unwrap_or_else(|| line.participant.clone());
        let mut board = self.board.lock().expect("client projection mutex poisoned");
        let Some(status) = board.participants.get_mut(&id) else {
            drop(board);
            self.disclose_unknown_bus_id(&id, "log");
            return false;
        };
        line.participant = id;
        status.last_log_line = Some(line.text.clone());
        drop(board);
        self.log_sink
            .lock()
            .expect("log sink mutex poisoned")
            .as_ref()
            .is_none_or(|sender| sender.try_send(RoutedLogUpdate::Append(line)).is_ok())
    }

    fn replace_bus_logs(&self, scope: LogScope, lines: Vec<RoutedLogLine>) -> bool {
        let mut accepted = Vec::with_capacity(lines.len());
        for mut line in lines {
            line.text = bounded_log_text(&line.text);
            if let Some(id) = self.display_id_for_bus_line(&line) {
                line.participant = id;
                let mut board = self.board.lock().expect("client projection mutex poisoned");
                if let Some(status) = board.participants.get_mut(&line.participant) {
                    status.last_log_line = Some(line.text.clone());
                    accepted.push(line);
                }
            } else {
                self.disclose_unknown_bus_id(&line.participant, "log");
            }
        }
        self.log_sink
            .lock()
            .expect("log sink mutex poisoned")
            .as_ref()
            .is_none_or(|sender| {
                sender
                    .try_send(RoutedLogUpdate::Replace {
                        scope,
                        lines: accepted,
                    })
                    .is_ok()
            })
    }

    fn display_id_for_bus_line(&self, line: &RoutedLogLine) -> Option<String> {
        let scope = line.scope.as_ref()?;
        let id = format!(
            "{}/{}::{}",
            scope.namespace, scope.robot_id, line.participant
        );
        self.board
            .lock()
            .expect("client projection mutex poisoned")
            .participants
            .contains_key(&id)
            .then_some(id)
    }

    fn disclose_unknown_bus_id(&self, id: &str, signal: &'static str) {
        const MAX_UNKNOWN_IDS: usize = 64;
        let mut disclosed = self
            .unknown_bus_ids
            .lock()
            .expect("unknown bus id mutex poisoned");
        if disclosed.len() < MAX_UNKNOWN_IDS && disclosed.insert(id.chars().take(128).collect()) {
            tracing::warn!(participant = %id, signal, "ignored bus traffic from an unplanned participant");
        }
    }
}

fn participant_state(state: phoxal_cli_core::session::ProcessState) -> ParticipantState {
    match state {
        phoxal_cli_core::session::ProcessState::Starting => ParticipantState::Starting,
        phoxal_cli_core::session::ProcessState::Ready => ParticipantState::Ready,
        phoxal_cli_core::session::ProcessState::Degraded => ParticipantState::Degraded,
        phoxal_cli_core::session::ProcessState::Restarting => ParticipantState::Restarting,
        phoxal_cli_core::session::ProcessState::Failed => ParticipantState::Failed,
        phoxal_cli_core::session::ProcessState::Stopped => ParticipantState::Stopped,
    }
}

pub fn start_bus_log_subscriber(
    namespace: String,
    robot_id: String,
    connect: String,
    execution: ExecutionId,
    projection: ClientProjection,
    mut recovery_epochs: watch::Receiver<u64>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let scope = LogScope {
            namespace: namespace.clone(),
            robot_id: robot_id.clone(),
        };
        loop {
            let bus = match Bus::open(BusConfig {
                namespace: namespace.clone(),
                robot_id: robot_id.clone(),
                participant: LOG_OBSERVER_PARTICIPANT.to_string(),
                execution,
                producer: ProducerId::mint(),
                connect_endpoints: vec![connect.clone()],
            })
            .await
            {
                Ok(bus) => bus,
                Err(error) => {
                    tracing::debug!("bus log subscriber waiting for router: {error}");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    continue;
                }
            };
            let subscriber = bus_log_subscriber_loop(&bus, &scope, projection.clone());
            tokio::pin!(subscriber);
            let result = tokio::select! {
                result = &mut subscriber => Some(result),
                changed = recovery_epochs.changed() => {
                    if changed.is_err() {
                        break;
                    }
                    tracing::debug!(
                        graph_generation = *recovery_epochs.borrow_and_update(),
                        "recreating tool-log transport after graph recovery"
                    );
                    None
                }
            };
            if let Err(error) = bus.close().await {
                tracing::debug!("bus log subscriber close failed: {error}");
            }
            if let Some(result) = result {
                let error = result.expect_err("bus log subscriber loop is intentionally endless");
                tracing::debug!("bus log subscriber waiting for router: {error:#}");
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    })
}

pub(crate) async fn bus_log_subscriber_loop(
    bus: &Bus,
    scope: &LogScope,
    projection: ClientProjection,
) -> Result<Infallible> {
    let follow_topic = api::topic::client().tool().log().follow();
    let subscriber = Subscriber::<api::tool::log::Follow>::new(bus, &follow_topic, 256).await?;
    let snapshot_topic = api::topic::client().tool().log().snapshot();
    let querier = Querier::<api::tool::log::SnapshotRequest, api::tool::log::Snapshot>::new(
        bus.clone(),
        &snapshot_topic,
        DEFAULT_QUERY_TIMEOUT,
    )?;
    let mut reconciler = Reconciler::new(512);
    let mut local_drops = subscriber.dropped();
    let mut tool_drops = 0_u64;
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
                    disclose_tool_log_loss(snapshot.ingest_dropped, &mut tool_drops);
                    let generation = snapshot.cursor.generation.clone();
                    let records = snapshot.records.into_iter().map(|record| RetainedLogFollow {
                        cursor: Cursor { generation: generation.clone(), sequence: record.sequence },
                        ingest_dropped: snapshot.ingest_dropped,
                        record,
                    }).collect();
                    let outcome = reconciler.install(
                        Cursor { generation: snapshot.cursor.generation, sequence: snapshot.cursor.sequence },
                        records,
                    );
                    if !apply_log_outcome(&projection, scope, outcome, &mut tool_drops) {
                        let _ = reconciler.local_drop();
                        prepare_log_requery(&subscriber, &mut local_drops, &mut retry_backoff).await;
                        continue 'query;
                    }
                    retry_backoff.reset();
                    break;
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
                    let follow_item = RetainedLogFollow::from(received.body);
                    disclose_tool_log_loss(follow_item.ingest_dropped, &mut tool_drops);
                    if matches!(reconciler.follow(follow_item), ReconcileOutcome::Requery) {
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
            let follow_item = RetainedLogFollow::from(received.body);
            disclose_tool_log_loss(follow_item.ingest_dropped, &mut tool_drops);
            let outcome = reconciler.follow(follow_item);
            if matches!(outcome, ReconcileOutcome::Requery)
                || !apply_log_outcome(&projection, scope, outcome, &mut tool_drops)
            {
                prepare_log_requery(&subscriber, &mut local_drops, &mut retry_backoff).await;
                continue 'query;
            }
        }
    }
}

pub fn start_presence_observer(
    namespace: String,
    robot_id: String,
    connect: String,
    execution: ExecutionId,
    projection: ClientProjection,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let result = presence_observer_loop(
                namespace.clone(),
                robot_id.clone(),
                connect.clone(),
                execution,
                projection.clone(),
            )
            .await;
            match result {
                Ok(()) => break,
                Err(error) => {
                    tracing::debug!("client presence observer waiting for router: {error:#}");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
        }
    })
}

async fn presence_observer_loop(
    namespace: String,
    robot_id: String,
    connect: String,
    execution: ExecutionId,
    projection: ClientProjection,
) -> Result<()> {
    let bus = Bus::open(BusConfig {
        namespace: namespace.clone(),
        robot_id: robot_id.clone(),
        participant: PRESENCE_OBSERVER_PARTICIPANT.to_string(),
        execution,
        producer: ProducerId::mint(),
        connect_endpoints: vec![connect],
    })
    .await
    .map_err(|error| anyhow!("failed to open client presence observer: {error}"))?;
    let _observer = bus
        .observe_participant_liveliness(move |event: ParticipantLivelinessEvent| {
            if !is_disposable_observer(event.key.participant()) {
                projection.record_presence(
                    &namespace,
                    &robot_id,
                    event.key.participant(),
                    event.status == ParticipantLivelinessStatus::Alive,
                );
            }
        })
        .await
        .map_err(|error| anyhow!("failed to observe participant presence: {error}"))?;
    std::future::pending::<()>().await;
    Ok(())
}

/// Start a background feed of simulation-clock samples for the attached TUI.
pub fn start_clock_feed(
    namespace: String,
    robot_id: String,
    connect: String,
    execution: ExecutionId,
) -> (watch::Receiver<ClockObservation>, JoinHandle<()>) {
    let (tx, rx) = watch::channel(ClockObservation::default());
    let handle = tokio::spawn(async move {
        loop {
            match clock_feed_loop(
                namespace.clone(),
                robot_id.clone(),
                connect.clone(),
                execution,
                &tx,
            )
            .await
            {
                Ok(()) => break,
                Err(error) => {
                    tracing::debug!("clock telemetry feed waiting for router: {error:#}");
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }
            }
        }
    });
    (rx, handle)
}

pub(crate) async fn clock_feed_loop(
    namespace: String,
    robot_id: String,
    connect: String,
    execution: ExecutionId,
    tx: &watch::Sender<ClockObservation>,
) -> Result<()> {
    let bus = Bus::open(BusConfig {
        namespace,
        robot_id,
        participant: CLOCK_OBSERVER_PARTICIPANT.to_string(),
        execution,
        producer: ProducerId::mint(),
        connect_endpoints: vec![connect],
    })
    .await
    .map_err(|error| anyhow!("failed to open bus clock subscription: {error}"))?;
    let topic = Topic::<Subscribe<api::simulation::Clock>>::new_static(
        <api::simulation::Clock as phoxal::bus::ContractBody>::TOPIC,
    );
    let subscriber = Subscriber::<api::simulation::Clock>::new(&bus, &topic, 32).await?;
    loop {
        let received = subscriber.recv().await?;
        tx.send_modify(|observation| {
            // The clock's instant rides in the envelope now, stamped by the
            // world authority; the body carries only the step counter.
            observation.latest = Some(ClockSample {
                now_ns: received
                    .metadata
                    .produced_exactly_at()
                    .map_or(0, |at| at.ticks()),
                step: received.body.step,
            });
            observation.received_at = Some(Instant::now());
        });
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

#[derive(Debug, Clone)]
struct RetainedLogFollow {
    cursor: Cursor,
    ingest_dropped: u64,
    record: api::tool::log::Record,
}

impl From<api::tool::log::Follow> for RetainedLogFollow {
    fn from(follow: api::tool::log::Follow) -> Self {
        Self {
            cursor: Cursor {
                generation: follow.cursor.generation,
                sequence: follow.cursor.sequence,
            },
            ingest_dropped: follow.ingest_dropped,
            record: follow.record,
        }
    }
}

impl Sequenced for RetainedLogFollow {
    fn cursor(&self) -> Cursor {
        self.cursor.clone()
    }
}

fn disclose_tool_log_loss(observed: u64, previous: &mut u64) {
    if observed > *previous {
        tracing::warn!(
            lost = observed.saturating_sub(*previous),
            cumulative = observed,
            "tool-log dropped structured events before retention"
        );
        *previous = observed;
    }
}

fn retained_log_line(scope: &LogScope, record: api::tool::log::Record) -> RoutedLogLine {
    let mut text = format!("{:?}: {}", record.level, record.message);
    if record.dropped > 0 {
        text.push_str(&format!(" (producer dropped {})", record.dropped));
    }
    if record.truncated > 0 {
        text.push_str(&format!(" (truncated {})", record.truncated));
    }
    let event_time = retained_log_time(&record.time);
    RoutedLogLine {
        participant: record.participant_id,
        source: LogSource::Bus,
        severity: match record.level {
            api::tool::log::Level::Error => LogSeverity::Error,
            api::tool::log::Level::Warn => LogSeverity::Warn,
            api::tool::log::Level::Info => LogSeverity::Info,
            api::tool::log::Level::Debug => LogSeverity::Debug,
            api::tool::log::Level::Trace => LogSeverity::Trace,
        },
        text: bounded_log_text(&text),
        event_time,
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

fn apply_log_outcome(
    projection: &ClientProjection,
    scope: &LogScope,
    outcome: ReconcileOutcome<RetainedLogFollow>,
    tool_drops: &mut u64,
) -> bool {
    match outcome {
        ReconcileOutcome::Installed { snapshot, replay } => {
            for item in &replay {
                disclose_tool_log_loss(item.ingest_dropped, tool_drops);
            }
            projection.replace_bus_logs(
                scope.clone(),
                snapshot
                    .into_iter()
                    .map(|item| retained_log_line(scope, item.record))
                    .collect(),
            ) && replay.into_iter().all(|item| {
                let line = retained_log_line(scope, item.record);
                projection.route_log_line(line)
            })
        }
        ReconcileOutcome::Append(item) => {
            let line = retained_log_line(scope, item.record);
            projection.route_log_line(line)
        }
        ReconcileOutcome::Buffered => true,
        ReconcileOutcome::Requery => false,
    }
}

#[cfg(test)]
mod projection_tests {
    use phoxal_cli_core::session::{
        ProcessDescriptor, ProcessEntry, ProcessKey, ProcessState, ProcessStatus, RobotKey,
        StartupRequirement,
    };

    use super::*;

    fn snapshot(key: ProcessKey) -> SupervisorSnapshotV0 {
        let mut snapshot = SupervisorSnapshotV0::default();
        snapshot.processes.insert(
            key.clone(),
            ProcessEntry {
                descriptor: ProcessDescriptor {
                    key,
                    kind: phoxal_cli_core::session::ParticipantKind::Service,
                    artifact: "drive".to_string(),
                    owner: "test".to_string(),
                    startup_requirement: StartupRequirement::Required,
                },
                status: ProcessStatus {
                    actual: ProcessState::Ready,
                    ..ProcessStatus::default()
                },
            },
        );
        snapshot
    }

    #[test]
    fn supervisor_replacement_retains_observation_within_one_graph() {
        let key = ProcessKey::robot(RobotKey::new("lab", "rover"), "drive");
        let projection = ClientProjection::default();
        let mut supervisor = snapshot(key);
        projection.replace_supervisor(&supervisor);
        projection.record_presence("lab", "rover", "drive", false);
        assert!(projection.route_log_line(RoutedLogLine {
            participant: "drive".to_string(),
            source: LogSource::Bus,
            severity: LogSeverity::Info,
            text: "latest".to_string(),
            event_time: SystemTime::now(),
            scope: Some(LogScope {
                namespace: "lab".to_string(),
                robot_id: "rover".to_string(),
            }),
        }));

        supervisor.revision += 1;
        projection.replace_supervisor(&supervisor);
        let row = &projection.snapshot().participants["lab/rover::drive"];
        assert_eq!(row.present, Some(false));
        assert_eq!(row.last_log_line.as_deref(), Some("latest"));

        supervisor.graph_generation += 1;
        projection.replace_supervisor(&supervisor);
        let row = &projection.snapshot().participants["lab/rover::drive"];
        assert_eq!(row.present, None);
        assert_eq!(row.last_log_line, None);
    }

    #[test]
    fn project_presence_fallback_and_log_text_bound_are_preserved() {
        let projection = ClientProjection::default();
        projection.replace_supervisor(&snapshot(ProcessKey::project("bridge")));
        projection.record_presence("lab", "rover", "bridge", true);
        assert_eq!(
            projection.snapshot().participants["bridge"].present,
            Some(true)
        );

        assert!(projection.route_log_line(RoutedLogLine {
            participant: "bridge".to_string(),
            source: LogSource::Raw,
            severity: LogSeverity::Info,
            text: "x".repeat(phoxal_cli_core::session::MAX_ROUTED_LOG_TEXT_CHARS + 100),
            event_time: SystemTime::now(),
            scope: None,
        }));
        let line = projection.snapshot().participants["bridge"]
            .last_log_line
            .clone()
            .unwrap();
        assert!(line.chars().count() <= phoxal_cli_core::session::MAX_ROUTED_LOG_TEXT_CHARS + 1);
        assert!(line.ends_with('…'));
    }

    #[test]
    fn unknown_presence_keeps_robot_scope_and_disposable_observers_are_hidden() {
        let projection = ClientProjection::default();
        projection.record_presence("lab", "rover", "ghost", true);
        assert!(
            projection
                .unknown_bus_ids
                .lock()
                .unwrap()
                .contains("lab/rover::ghost")
        );
        assert!(is_disposable_observer(LOG_OBSERVER_PARTICIPANT));
        assert!(is_disposable_observer(PRESENCE_OBSERVER_PARTICIPANT));
        assert!(is_disposable_observer(CLOCK_OBSERVER_PARTICIPANT));
        assert!(is_disposable_observer(JOYPAD_TELEMETRY_PARTICIPANT));
        assert!(is_disposable_observer(CONTROL_STATE_PARTICIPANT));
        assert!(is_disposable_observer(DEVICE_TELEMETRY_PARTICIPANT));
        assert!(is_disposable_observer(BUS_TELEMETRY_PARTICIPANT));
        assert!(is_disposable_observer(RUNTIME_TELEMETRY_PARTICIPANT));
        assert!(!is_disposable_observer("drive"));
    }
}

#[cfg(test)]
mod retained_log_tests {
    use super::*;

    #[test]
    fn retained_log_time_preserves_producer_timestamp() {
        let timestamp = api::tool::log::Timestamp {
            unix_seconds: 12,
            nanos: 345,
        };
        assert_eq!(
            retained_log_time(&timestamp),
            UNIX_EPOCH + Duration::new(12, 345)
        );
    }
}
