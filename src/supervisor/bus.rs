//! Raw bus adapters for logs, presence, clock, and endpoint reachability.

use super::{BoardBackend, BoardSnapshot, LogSource, ParticipantState, log_severity};
use anyhow::Result;
use anyhow::anyhow;
use phoxal::bus::Subscribe;
use phoxal::bus::Subscriber;
use phoxal::bus::Topic;
use phoxal::raw::{Bus, BusConfig};
use phoxal_api::v1 as api;
use phoxal_api::v2 as preview_api;
use phoxal_cli_core::project::launch_plan::DEFAULT_ROUTER_CONNECT;
use phoxal_cli_core::session::telemetry::ClockObservation;
use phoxal_cli_core::session::telemetry::ClockSample;
use std::net::TcpStream;
use std::net::ToSocketAddrs;
use std::time::Duration;
use std::time::Instant;
use tokio::sync::watch;
use tokio::task::JoinHandle;

pub fn endpoint_reachable(endpoint: &str, timeout: Duration) -> bool {
    let Some(address) = endpoint.strip_prefix("tcp/") else {
        return false;
    };
    let Ok(mut addresses) = address.to_socket_addrs() else {
        return false;
    };
    let Some(address) = addresses.next() else {
        return false;
    };
    TcpStream::connect_timeout(&address, timeout).is_ok()
}

/// Wait for a TCP router endpoint before asking Zenoh to open a session.
/// Managed sessions intentionally start their observer feeds before the
/// router process so they cannot miss early readiness. A cheap TCP preflight
/// keeps those expected retries from producing Zenoh connection warnings on
/// top of the alternate-screen TUI.
pub(crate) async fn wait_for_endpoint(endpoint: &str) {
    while !endpoint_reachable(endpoint, Duration::from_millis(50)) {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

pub fn start_bus_log_subscriber(
    namespace: String,
    robot_id: String,
    connect: String,
    board: BoardBackend,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            wait_for_endpoint(&connect).await;
            match bus_log_subscriber_loop(
                namespace.clone(),
                robot_id.clone(),
                connect.clone(),
                board.clone(),
            )
            .await
            {
                Ok(()) => break,
                Err(error) => {
                    tracing::debug!("bus log subscriber waiting for router: {error:#}");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
        }
    })
}

pub(crate) async fn bus_log_subscriber_loop(
    namespace: String,
    robot_id: String,
    connect: String,
    board: BoardBackend,
) -> Result<()> {
    let bus = Bus::open(BusConfig {
        namespace,
        robot_id,
        participant: "phoxal-cli-supervisor".to_string(),
        incarnation: 0,
        connect_endpoints: vec![connect],
    })
    .await
    .map_err(|error| anyhow!("failed to open bus log subscription: {error}"))?;
    let topic = Topic::<Subscribe<api::logs::Event>>::new_owned(logs_wildcard_topic_key());
    let subscriber = Subscriber::<api::logs::Event>::new(&bus, &topic, 128).await?;
    loop {
        let received = subscriber.recv().await?;
        let id = received.metadata.source.participant;
        board.route_log_with_severity(
            &id,
            LogSource::Bus,
            log_severity(received.body.level),
            render_log_event(&received.body),
        );
    }
}

/// The `logs/{participant_id}` contract's version-qualified wildcard key,
/// e.g. `v1/logs/*`. `logs::Event::TOPIC` (`ContractBody::TOPIC`) is the
/// per-participant literal `v1/logs/{participant_id}`, which is not
/// itself subscribable across every participant - building the key from
/// `ContractBody::VERSION` instead of hand-writing the version prefix
/// keeps this in lockstep with the api tree if the version ever changes.
#[must_use]
pub fn logs_wildcard_topic_key() -> String {
    format!(
        "{}/logs/*",
        <api::logs::Event as phoxal::bus::ContractBody>::VERSION
    )
}

/// Subscribe every participant's `presence/heartbeat` on one robot's bus and
/// drive the board's OBSERVED readiness from it (`BoardBackend::record_heartbeat`),
/// mirroring `start_bus_log_subscriber`. Unlike `logs/{participant_id}`,
/// `presence/heartbeat` is a single static (non-wildcarded) topic that every
/// participant publishes to, told apart only by `metadata.source.participant` -
/// see `phoxal-api`'s `presence` node.
pub fn start_presence_heartbeat_subscriber(
    namespace: String,
    robot_id: String,
    connect: String,
    board: BoardBackend,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            wait_for_endpoint(&connect).await;
            match presence_heartbeat_subscriber_loop(
                namespace.clone(),
                robot_id.clone(),
                connect.clone(),
                board.clone(),
            )
            .await
            {
                Ok(()) => break,
                Err(error) => {
                    tracing::debug!("presence heartbeat subscriber waiting for router: {error:#}");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
        }
    })
}

pub(crate) async fn presence_heartbeat_subscriber_loop(
    namespace: String,
    robot_id: String,
    connect: String,
    board: BoardBackend,
) -> Result<()> {
    let bus = Bus::open(BusConfig {
        namespace,
        robot_id,
        participant: "phoxal-cli-supervisor-presence".to_string(),
        incarnation: 0,
        connect_endpoints: vec![connect],
    })
    .await
    .map_err(|error| anyhow!("failed to open bus presence subscription: {error}"))?;
    let topic = Topic::<Subscribe<api::presence::Heartbeat>>::new_static(
        <api::presence::Heartbeat as phoxal::bus::ContractBody>::TOPIC,
    );
    let subscriber = Subscriber::<api::presence::Heartbeat>::new(&bus, &topic, 128).await?;
    loop {
        let received = subscriber.recv().await?;
        // The body carries `participant` too (redundant with the metadata
        // source), but `metadata.source.participant` is the framework-stamped
        // identity - the same field the log subscriber trusts - so prefer it.
        let id = received.metadata.source.participant;
        board.record_heartbeat(&id, received.body.readiness);
    }
}

/// Start a background feed of `v2::simulation::Clock` samples. Returns a
/// `watch::Receiver` the TUI's telemetry layer polls cheaply, plus the feed
/// task's handle.
pub fn start_clock_feed(
    namespace: String,
    robot_id: String,
    connect: String,
) -> (watch::Receiver<ClockObservation>, JoinHandle<()>) {
    let (tx, rx) = watch::channel(ClockObservation::default());
    let handle = tokio::spawn(async move {
        loop {
            wait_for_endpoint(&connect).await;
            match clock_feed_loop(namespace.clone(), robot_id.clone(), connect.clone(), &tx).await {
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
    tx: &watch::Sender<ClockObservation>,
) -> Result<()> {
    let bus = Bus::open(BusConfig {
        namespace,
        robot_id,
        participant: "phoxal-cli-clock-observer".to_string(),
        incarnation: 0,
        connect_endpoints: vec![connect],
    })
    .await
    .map_err(|error| anyhow!("failed to open bus clock subscription: {error}"))?;
    let topic = Topic::<Subscribe<preview_api::simulation::Clock>>::new_static(
        <preview_api::simulation::Clock as phoxal::bus::ContractBody>::TOPIC,
    );
    let subscriber = Subscriber::<preview_api::simulation::Clock>::new(&bus, &topic, 32).await?;
    loop {
        let received = subscriber.recv().await?;
        tx.send_modify(|observation| {
            observation.latest = Some(ClockSample {
                now_ns: received.body.now_ns,
                step: received.body.step,
            });
            observation.received_at = Some(Instant::now());
        });
    }
}

/// Ids in `expected_bus_ids` not yet observed `Ready` on the board.
pub(crate) fn missing_ready_participants(
    board: &BoardSnapshot,
    expected_bus_ids: &[String],
) -> Vec<String> {
    expected_bus_ids
        .iter()
        .filter(|id| {
            !board
                .participants
                .get(id.as_str())
                .is_some_and(|status| status.state == ParticipantState::Ready)
        })
        .cloned()
        .collect()
}

pub(crate) fn failed_expected_participants(
    board: &BoardSnapshot,
    expected_bus_ids: &[String],
) -> Vec<String> {
    expected_bus_ids
        .iter()
        .filter(|id| {
            board
                .participants
                .get(id.as_str())
                .is_some_and(|status| status.state == ParticipantState::Failed)
        })
        .cloned()
        .collect()
}

#[must_use]
pub fn render_log_event(event: &api::logs::Event) -> String {
    let mut message = format!("{:?}: {}", event.level, event.message);
    if event.dropped > 0 {
        message.push_str(&format!(" (dropped {})", event.dropped));
    }
    message
}

#[must_use]
pub fn default_connect_endpoint() -> String {
    DEFAULT_ROUTER_CONNECT.to_string()
}
