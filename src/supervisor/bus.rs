//! Raw bus adapters for logs, Liveliness, and clock.

#[cfg(test)]
use super::ParticipantState;
use super::{BoardBackend, LogSource};
use anyhow::Result;
use anyhow::anyhow;
use phoxal::bus::{DEFAULT_QUERY_TIMEOUT, Querier, Subscribe, Subscriber, Topic};
use phoxal::raw::{Bus, BusConfig};
use phoxal::raw::{ParticipantLivelinessEvent, ParticipantLivelinessStatus};
use phoxal_api::v0_1 as api;
use phoxal_cli_core::identity::{ExecutionId, ProducerId};
use phoxal_cli_core::project::launch_plan::DEFAULT_ROUTER_CONNECT;
use phoxal_cli_core::session::reconcile::{
    Cursor, ReconcileOutcome, Reconciler, RetryBackoff, Sequenced,
};
use phoxal_cli_core::session::telemetry::{ClockObservation, ClockSample};
use phoxal_cli_core::session::{LogScope, LogSeverity, RoutedLogLine};
use phoxal_cli_core::session::{ParticipantInstanceKey, RobotKey};
use std::convert::Infallible;
use std::time::{Duration, Instant};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::watch;
use tokio::task::JoinHandle;

pub fn start_bus_log_subscriber(
    namespace: String,
    robot_id: String,
    connect: String,
    execution: ExecutionId,
    board: BoardBackend,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut recovery_epochs = board.recovery_epoch_receiver();
        let scope = LogScope {
            namespace: namespace.clone(),
            robot_id: robot_id.clone(),
        };
        loop {
            let bus = match Bus::open(BusConfig {
                namespace: namespace.clone(),
                robot_id: robot_id.clone(),
                participant: "phoxal-cli-supervisor".to_string(),
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
            let subscriber = bus_log_subscriber_loop(&bus, &scope, board.clone());
            tokio::pin!(subscriber);
            let result = tokio::select! {
                result = &mut subscriber => Some(result),
                changed = recovery_epochs.changed() => {
                    if changed.is_err() {
                        break;
                    }
                    tracing::debug!(
                        recovery_epoch = *recovery_epochs.borrow_and_update(),
                        "recreating tool-log snapshot/follow transport after graph recovery"
                    );
                    None
                }
            };
            if let Err(error) = bus.close().await {
                tracing::debug!("bus log subscriber close failed: {error}");
            }
            match result {
                None => continue,
                Some(result) => {
                    let error =
                        result.expect_err("bus log subscriber loop is intentionally endless");
                    tracing::debug!("bus log subscriber waiting for router: {error:#}");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
        }
    })
}

pub(crate) async fn bus_log_subscriber_loop(
    bus: &Bus,
    scope: &LogScope,
    board: BoardBackend,
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
                    if !apply_log_outcome(&board, scope, outcome, &mut tool_drops) {
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
                || !apply_log_outcome(&board, scope, outcome, &mut tool_drops)
            {
                prepare_log_requery(&subscriber, &mut local_drops, &mut retry_backoff).await;
                continue 'query;
            }
        }
    }
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
        participant: "phoxal-cli-clock-observer".to_string(),
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
        text,
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

fn apply_log_outcome(
    board: &BoardBackend,
    scope: &LogScope,
    outcome: ReconcileOutcome<RetainedLogFollow>,
    tool_drops: &mut u64,
) -> bool {
    match outcome {
        ReconcileOutcome::Installed { snapshot, replay } => {
            for item in &replay {
                disclose_tool_log_loss(item.ingest_dropped, tool_drops);
            }
            board.replace_bus_logs(
                scope.clone(),
                snapshot
                    .into_iter()
                    .map(|item| retained_log_line(scope, item.record))
                    .collect(),
            ) && replay.into_iter().all(|item| {
                let line = retained_log_line(scope, item.record);
                board.route_log_line(line)
            })
        }
        ReconcileOutcome::Append(item) => {
            let line = retained_log_line(scope, item.record);
            board.route_log_line(line)
        }
        ReconcileOutcome::Buffered => true,
        ReconcileOutcome::Requery => false,
    }
}

const LIVELINESS_OBSERVER_ID: &str = "phoxal-cli-liveliness-observer";

/// Observe every planned participant's stable Zenoh Liveliness key on one
/// robot bus. Callers register the finite participant set on the board before
/// starting this observer; traffic for any other key is deliberately ignored.
/// History is enabled by the framework wrapper, so participants that completed
/// setup before this observer connected are discovered immediately.
pub fn start_liveliness_observer(
    namespace: String,
    robot_id: String,
    connect: String,
    execution: ExecutionId,
    board: BoardBackend,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            match liveliness_observer_loop(
                namespace.clone(),
                robot_id.clone(),
                connect.clone(),
                execution,
                board.clone(),
            )
            .await
            {
                Ok(()) => break,
                Err(error) => {
                    tracing::debug!("liveliness observer waiting for router: {error:#}");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
        }
    })
}

pub(crate) async fn liveliness_observer_loop(
    namespace: String,
    robot_id: String,
    connect: String,
    execution: ExecutionId,
    board: BoardBackend,
) -> Result<()> {
    let bus = Bus::open(BusConfig {
        namespace: namespace.clone(),
        robot_id: robot_id.clone(),
        participant: LIVELINESS_OBSERVER_ID.to_string(),
        execution,
        producer: ProducerId::mint(),
        connect_endpoints: vec![connect],
    })
    .await
    .map_err(|error| anyhow!("failed to open bus Liveliness observer: {error}"))?;
    let _observer = bus
        .observe_participant_liveliness(move |event| {
            apply_liveliness_event(&board, &namespace, &robot_id, event);
        })
        .await
        .map_err(|error| anyhow!("failed to observe participant Liveliness: {error}"))?;
    // Once declared, the Bus session and Zenoh subscriber own transparent
    // transport reconnection. The outer loop above retries only initial open
    // or declaration failures; there is no application-level heartbeat loop.
    std::future::pending::<()>().await;
    Ok(())
}

fn apply_liveliness_event(
    board: &BoardBackend,
    namespace: &str,
    robot_id: &str,
    event: ParticipantLivelinessEvent,
) {
    let id = event.key.participant();
    // Participant ids are the launch plan's validated, robot-scoped flat
    // namespace. The framework documents that a session observing a key it also holds
    // can receive an uncompensated self-Lost after duplicate-key
    // reconciliation. This observer does not normally declare a token, but
    // filtering its own id keeps that invariant explicit.
    if id == LIVELINESS_OBSERVER_ID {
        return;
    }
    board.record_instance_presence(
        ParticipantInstanceKey {
            robot: RobotKey::new(namespace, robot_id),
            participant: id.to_string(),
            producer: event.key.producer(),
        },
        event.status == ParticipantLivelinessStatus::Alive,
    );
}

#[must_use]
pub fn default_connect_endpoint() -> String {
    DEFAULT_ROUTER_CONNECT.to_string()
}

#[cfg(test)]
mod tests {

    /// A deterministic producer identity for tests, so a case can name the
    /// exact restart it means.
    fn producer(seed: u8) -> ProducerId {
        ProducerId::parse(&format!("{:032x}", u128::from(seed)))
            .expect("test producer id must parse")
    }
    use super::*;
    use phoxal::raw::ParticipantLivelinessKey;
    use phoxal_cli_core::session::ParticipantKind;

    fn event(participant: &str, status: ParticipantLivelinessStatus) -> ParticipantLivelinessEvent {
        ParticipantLivelinessEvent {
            key: ParticipantLivelinessKey::new("dev/robots/rover/xdead", participant, producer(7))
                .expect("valid participant key"),
            status,
        }
    }

    #[test]
    fn observer_events_drive_presence_without_becoming_restart_authority() {
        let board = BoardBackend::new();
        let key = phoxal_cli_core::session::ProcessKey::robot(
            phoxal_cli_core::session::RobotKey::new("dev", "rover"),
            "drive",
        );
        board.register_planned(
            &key,
            ParticipantKind::Service,
            phoxal_cli_core::session::StartupRequirement::Required,
        );
        board.set_producer(&key, producer(7));

        apply_liveliness_event(
            &board,
            "dev",
            "rover",
            event("drive", ParticipantLivelinessStatus::Alive),
        );
        assert_eq!(
            board.snapshot().participants["dev/rover::drive"].state,
            ParticipantState::Ready
        );

        apply_liveliness_event(
            &board,
            "dev",
            "rover",
            event("drive", ParticipantLivelinessStatus::Lost),
        );
        assert_eq!(
            board.snapshot().participants["dev/rover::drive"].state,
            ParticipantState::Ready,
            "Lost is observational and must not mutate process lifecycle"
        );
    }

    #[test]
    fn observer_filters_its_own_participant_id() {
        // Synthetic guard coverage: the observer currently holds no
        // Liveliness token, but its reserved id must never become a board row.
        let board = BoardBackend::new();
        let key = phoxal_cli_core::session::ProcessKey::robot(
            phoxal_cli_core::session::RobotKey::new("dev", "rover"),
            LIVELINESS_OBSERVER_ID,
        );
        board.register_planned(
            &key,
            ParticipantKind::Tool,
            phoxal_cli_core::session::StartupRequirement::Optional,
        );
        apply_liveliness_event(
            &board,
            "dev",
            "rover",
            event(LIVELINESS_OBSERVER_ID, ParticipantLivelinessStatus::Alive),
        );
        assert_eq!(
            board.snapshot().participants["dev/rover::phoxal-cli-liveliness-observer"].state,
            ParticipantState::Starting
        );
    }
}
