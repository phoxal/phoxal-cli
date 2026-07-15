//! Live telemetry feed for the TUI (CLI-UX Phase 3/4): background bus
//! subscribers for the framework's `v2` router/telemetry/joypad
//! contracts, plus the simulation clock's live step/time readout,
//! mirroring `supervisor::start_bus_log_subscriber`/
//! `start_presence_heartbeat_subscriber`'s "subscribe, update a shared
//! snapshot" pattern.
//!
//! Kept deliberately separate from `supervisor::BoardBackend`/`BoardSnapshot`
//! (participant board state, persisted to the state file and surfaced by
//! `--message-format json`): telemetry never reaches either of those, only
//! the live TUI reads it (`TelemetryBackend::snapshot`), so it can carry
//! whatever shape is convenient for rendering without touching the
//! JSON-stable board contract.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use phoxal::bus::{ContractBody, LogicalTime, Publish, Publisher, Subscribe, Subscriber, Topic};
use phoxal::raw::{Bus, BusConfig};
use phoxal_api::{v1 as state_api, v2 as api};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

use crate::stores::telemetry_store::{HostPoint, TelemetryStore, Timestamped};
use crate::supervisor::{ClockObservation, wait_for_endpoint};

/// One `telemetry::Host` sample (the host resource meter).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct HostSample {
    pub cpu_pct: f32,
    pub ram_used_bytes: u64,
    pub ram_total_bytes: u64,
    pub load_1m: f32,
    pub window_ns: u64,
}

impl From<api::telemetry::Host> for HostSample {
    fn from(body: api::telemetry::Host) -> Self {
        Self {
            cpu_pct: body.cpu_pct,
            ram_used_bytes: body.ram_used_bytes,
            ram_total_bytes: body.ram_total_bytes,
            load_1m: body.load_1m,
            window_ns: body.window_ns,
        }
    }
}

/// One `telemetry::Process` sample for a single participant, demuxed by
/// envelope source (`received.metadata.source.participant`) exactly like
/// `supervisor::presence_heartbeat_subscriber_loop` demuxes
/// `presence::Heartbeat`.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ProcessSample {
    pub cpu_pct: f32,
    pub rss_bytes: u64,
    pub window_ns: u64,
}

impl From<api::telemetry::Process> for ProcessSample {
    fn from(body: api::telemetry::Process) -> Self {
        Self {
            cpu_pct: body.cpu_pct,
            rss_bytes: body.rss_bytes,
            window_ns: body.window_ns,
        }
    }
}

/// One row of the router Traffic table.
#[derive(Debug, Clone, PartialEq)]
pub struct TopicMetric {
    pub topic: String,
    pub from_participant: String,
    pub ingress_rate_hz: f32,
    pub count: u64,
}

impl From<api::router::TopicMetric> for TopicMetric {
    fn from(body: api::router::TopicMetric) -> Self {
        Self {
            topic: body.topic,
            from_participant: body.from_participant,
            ingress_rate_hz: body.ingress_rate_hz,
            count: body.count,
        }
    }
}

/// The router's latest `router::Metrics` sample.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RouterMetricsSample {
    pub topics: Vec<TopicMetric>,
    pub throughput_msg_s: f32,
    pub window_ns: u64,
}

impl From<api::router::Metrics> for RouterMetricsSample {
    fn from(body: api::router::Metrics) -> Self {
        Self {
            topics: body.topics.into_iter().map(TopicMetric::from).collect(),
            throughput_msg_s: body.throughput_msg_s,
            window_ns: body.window_ns,
        }
    }
}

/// One gamepad the joypad tool can see.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct JoypadDevice {
    pub id: String,
    pub name: String,
    pub connected: bool,
}

impl From<api::joypad::Device> for JoypadDevice {
    fn from(body: api::joypad::Device) -> Self {
        Self {
            id: body.id,
            name: body.name,
            connected: body.connected,
        }
    }
}

/// The joypad tool's latest published device state - `selected` is the
/// AUTHORITATIVE selection (the tool's own ack), never a local UI guess; see
/// `tui::state::DevicesState::cursor` for the separate, purely local list
/// cursor.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct JoypadDevicesSample {
    pub available: Vec<JoypadDevice>,
    pub selected: Option<String>,
    pub last_error: Option<String>,
}

impl From<api::joypad::Devices> for JoypadDevicesSample {
    fn from(body: api::joypad::Devices) -> Self {
        Self {
            available: body.available.into_iter().map(JoypadDevice::from).collect(),
            selected: body.selected,
            last_error: body.last_error,
        }
    }
}

/// A snapshot of every live telemetry feed, cloned once per TUI redraw
/// (mirrors `BoardBackend::snapshot`). Every latest-value field is a
/// [`Timestamped`] carrying the [`Instant`] the underlying sample was
/// actually RECEIVED off the bus (recorded by `TelemetryBackend::record_*`
/// at the moment a feed task observes it, never re-stamped on later
/// redraws), so a renderer can ask `Timestamped::is_stale`/`freshness`
/// instead of trusting a long-cached value as if it were still live - see
/// `stores::telemetry_store`'s module docs for the two bugs this fixes.
#[derive(Debug, Clone, Default)]
pub struct TelemetrySnapshot {
    /// The simulation clock's latest observed sample - `None` in `run` mode
    /// (no clock feed is ever started there) or before the first sample
    /// arrives in `simulation` mode.
    pub clock: Option<crate::supervisor::ClockSample>,
    /// `None` until `tool-telemetry`'s first `Host` sample arrives - this is
    /// the graceful-absence case (the tool may not be in the catalog yet),
    /// rendered as `cpu n/a` rather than a failure.
    pub host: Option<Timestamped<HostSample>>,
    /// The Resources tab's rolling host-sample history, oldest first -
    /// advances on every received sample regardless of whether its payload
    /// differs from the previous one (see `TelemetryStore::record_host`'s
    /// docs on the value-dedup bug this replaces).
    pub host_history: Vec<HostPoint>,
    pub process_by_participant: BTreeMap<String, Timestamped<ProcessSample>>,
    pub router: Option<Timestamped<RouterMetricsSample>>,
    pub joypad: Option<Timestamped<JoypadDevicesSample>>,
    pub motion: Option<Timestamped<state_api::motion::State>>,
    pub drive: Option<Timestamped<state_api::drive::State>>,
}

/// A joypad action the TUI's Devices tab asked to publish.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JoypadCommand {
    Connect(String),
    Rescan,
}

/// Live-telemetry state shared between the background feed tasks
/// (`start_host_feed` etc.) and the TUI's redraw path
/// (`TuiDisplay::redraw`/`render::draw`). Cheap to clone (an `Arc` handle);
/// every feed task and the TUI hold their own clone.
///
/// Backed by [`TelemetryStore`] (Wave C2): every `record_*` call below
/// stamps the sample with `Instant::now()` AT THE MOMENT the feed task
/// received it, not when a later redraw happens to observe it - that
/// receive-time timestamp is what makes [`TelemetrySnapshot`]'s freshness
/// checks meaningful.
#[derive(Debug, Clone, Default)]
pub struct TelemetryBackend {
    inner: Arc<Mutex<TelemetryStore>>,
    clock_rx: Arc<Mutex<Option<watch::Receiver<ClockObservation>>>>,
    joypad_command_tx: Arc<Mutex<Option<mpsc::Sender<JoypadCommand>>>>,
}

/// A generous bound for a user-driven command channel (one send per
/// keypress in the Devices tab, never a hot loop): [`JoypadCommand`]s queue
/// here between redraws, so this only needs to absorb a rapid burst of
/// key presses, not hold unbounded history.
const JOYPAD_COMMAND_CHANNEL_CAPACITY: usize = 16;

impl TelemetryBackend {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Wire in the simulation clock feed
    /// (`supervisor::start_clock_feed`). The caller keeps the feed alive for
    /// the session and hands the receiver here for cheap redraw snapshots.
    pub fn set_clock_feed(&self, rx: watch::Receiver<ClockObservation>) {
        *self.clock_rx.lock().expect("clock_rx mutex poisoned") = Some(rx);
    }

    fn set_joypad_command_sender(&self, tx: mpsc::Sender<JoypadCommand>) {
        *self
            .joypad_command_tx
            .lock()
            .expect("joypad_command_tx mutex poisoned") = Some(tx);
    }

    /// Publish a joypad `Connect`/`Rescan` command, requested from the TUI's
    /// Devices tab. A silent no-op if no joypad feed is running (the tool is
    /// absent from the catalog, or the session predates the feed starting),
    /// or if the command channel is full (newest-wins: an overloaded queue
    /// means the feed loop is stalled, so a fresh command is not worth
    /// blocking the TUI's input-handling path over) - there is nothing
    /// actionable to report to the user beyond what the Devices tab already
    /// shows (no device list at all).
    pub fn send_joypad_command(&self, command: JoypadCommand) {
        if let Some(sender) = &*self
            .joypad_command_tx
            .lock()
            .expect("joypad_command_tx mutex poisoned")
        {
            let _ = sender.try_send(command);
        }
    }

    #[must_use]
    pub fn snapshot(&self) -> TelemetrySnapshot {
        let mut store = self.inner.lock().expect("telemetry mutex poisoned");
        let mut snapshot = TelemetrySnapshot {
            clock: None,
            host: store.host().cloned(),
            host_history: store.host_history().to_vec(),
            process_by_participant: store.process_all().clone(),
            router: store.router().cloned(),
            joypad: store.joypad().cloned(),
            motion: store.motion().cloned(),
            drive: store.drive().cloned(),
        };
        drop(store);
        if let Some(rx) = &*self.clock_rx.lock().expect("clock_rx mutex poisoned") {
            snapshot.clock = rx.borrow().latest;
        }
        snapshot
    }

    fn record_host(&self, sample: HostSample) {
        self.inner
            .lock()
            .expect("telemetry mutex poisoned")
            .record_host(Instant::now(), sample);
    }

    fn record_process(&self, participant: String, sample: ProcessSample) {
        self.inner
            .lock()
            .expect("telemetry mutex poisoned")
            .record_process(Instant::now(), participant, sample);
    }

    fn record_router(&self, sample: RouterMetricsSample) {
        self.inner
            .lock()
            .expect("telemetry mutex poisoned")
            .record_router(Instant::now(), sample);
    }

    fn record_joypad(&self, sample: JoypadDevicesSample) {
        self.inner
            .lock()
            .expect("telemetry mutex poisoned")
            .record_joypad(Instant::now(), sample);
    }

    fn record_motion(&self, sample: state_api::motion::State) {
        self.inner
            .lock()
            .expect("telemetry mutex poisoned")
            .record_motion(Instant::now(), sample);
    }

    fn record_drive(&self, sample: state_api::drive::State) {
        self.inner
            .lock()
            .expect("telemetry mutex poisoned")
            .record_drive(Instant::now(), sample);
    }
}

pub fn start_control_state_feed(
    namespace: String,
    robot_id: String,
    connect: String,
    telemetry: TelemetryBackend,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            wait_for_endpoint(&connect).await;
            if let Err(error) = control_state_feed_loop(
                namespace.clone(),
                robot_id.clone(),
                connect.clone(),
                &telemetry,
            )
            .await
            {
                tracing::debug!("control-state feed waiting for router: {error:#}");
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    })
}

async fn control_state_feed_loop(
    namespace: String,
    robot_id: String,
    connect: String,
    telemetry: &TelemetryBackend,
) -> Result<()> {
    let bus = Bus::open(BusConfig {
        namespace,
        robot_id,
        participant: "phoxal-cli-control-state".to_string(),
        incarnation: 0,
        connect_endpoints: vec![connect],
    })
    .await?;
    let motion_topic = Topic::<Subscribe<state_api::motion::State>>::new_static(
        <state_api::motion::State as ContractBody>::TOPIC,
    );
    let drive_topic = Topic::<Subscribe<state_api::drive::State>>::new_static(
        <state_api::drive::State as ContractBody>::TOPIC,
    );
    let motion = Subscriber::new(&bus, &motion_topic, 32).await?;
    let drive = Subscriber::new(&bus, &drive_topic, 32).await?;
    loop {
        tokio::select! {
            received = motion.recv() => telemetry.record_motion(received?.body),
            received = drive.recv() => telemetry.record_drive(received?.body),
        }
    }
}

fn now() -> LogicalTime {
    let elapsed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    LogicalTime::new(0, u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX))
}

/// Subscribe `v2::telemetry::Host` and feed every sample into
/// `telemetry`, mirroring `supervisor::start_presence_heartbeat_subscriber`'s
/// "open bus, subscribe, loop `recv`, retry on transport error" shape.
/// Absence is expected and graceful: `tool-telemetry` may not be in the
/// catalog yet, in which case this task simply never observes a sample and
/// `TelemetrySnapshot::host` stays `None` forever - the caller does not treat
/// that as an error.
pub fn start_host_feed(
    namespace: String,
    robot_id: String,
    connect: String,
    telemetry: TelemetryBackend,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            wait_for_endpoint(&connect).await;
            match host_feed_loop(
                namespace.clone(),
                robot_id.clone(),
                connect.clone(),
                &telemetry,
            )
            .await
            {
                Ok(()) => break,
                Err(error) => {
                    tracing::debug!("telemetry host feed waiting for router: {error:#}");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
        }
    })
}

async fn host_feed_loop(
    namespace: String,
    robot_id: String,
    connect: String,
    telemetry: &TelemetryBackend,
) -> Result<()> {
    let bus = Bus::open(BusConfig {
        namespace,
        robot_id,
        participant: "phoxal-cli-telemetry-host".to_string(),
        incarnation: 0,
        connect_endpoints: vec![connect],
    })
    .await
    .map_err(|error| anyhow!("failed to open bus telemetry/host subscription: {error}"))?;
    let topic = Topic::<Subscribe<api::telemetry::Host>>::new_static(
        <api::telemetry::Host as ContractBody>::TOPIC,
    );
    let subscriber = Subscriber::<api::telemetry::Host>::new(&bus, &topic, 32).await?;
    loop {
        let received = subscriber.recv().await?;
        telemetry.record_host(received.body.into());
    }
}

/// Subscribe `v2::telemetry::Process` and demux by envelope source
/// participant, exactly like
/// `supervisor::presence_heartbeat_subscriber_loop` demuxes
/// `presence::Heartbeat`: every participant publishes its OWN process sample
/// to the same static topic, told apart only by `metadata.source.participant`.
pub fn start_process_feed(
    namespace: String,
    robot_id: String,
    connect: String,
    telemetry: TelemetryBackend,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            wait_for_endpoint(&connect).await;
            match process_feed_loop(
                namespace.clone(),
                robot_id.clone(),
                connect.clone(),
                &telemetry,
            )
            .await
            {
                Ok(()) => break,
                Err(error) => {
                    tracing::debug!("telemetry process feed waiting for router: {error:#}");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
        }
    })
}

async fn process_feed_loop(
    namespace: String,
    robot_id: String,
    connect: String,
    telemetry: &TelemetryBackend,
) -> Result<()> {
    let bus = Bus::open(BusConfig {
        namespace,
        robot_id,
        participant: "phoxal-cli-telemetry-process".to_string(),
        incarnation: 0,
        connect_endpoints: vec![connect],
    })
    .await
    .map_err(|error| anyhow!("failed to open bus telemetry/process subscription: {error}"))?;
    let topic = Topic::<Subscribe<api::telemetry::Process>>::new_static(
        <api::telemetry::Process as ContractBody>::TOPIC,
    );
    let subscriber = Subscriber::<api::telemetry::Process>::new(&bus, &topic, 128).await?;
    loop {
        let received = subscriber.recv().await?;
        let participant = received.metadata.source.participant.clone();
        telemetry.record_process(participant, received.body.into());
    }
}

/// Subscribe `v2::router::Metrics` (the Traffic tab's data source).
pub fn start_router_metrics_feed(
    namespace: String,
    robot_id: String,
    connect: String,
    telemetry: TelemetryBackend,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            wait_for_endpoint(&connect).await;
            match router_metrics_feed_loop(
                namespace.clone(),
                robot_id.clone(),
                connect.clone(),
                &telemetry,
            )
            .await
            {
                Ok(()) => break,
                Err(error) => {
                    tracing::debug!("router metrics feed waiting for router: {error:#}");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
        }
    })
}

async fn router_metrics_feed_loop(
    namespace: String,
    robot_id: String,
    connect: String,
    telemetry: &TelemetryBackend,
) -> Result<()> {
    let bus = Bus::open(BusConfig {
        namespace,
        robot_id,
        participant: "phoxal-cli-telemetry-router".to_string(),
        incarnation: 0,
        connect_endpoints: vec![connect],
    })
    .await
    .map_err(|error| anyhow!("failed to open bus router/metrics subscription: {error}"))?;
    let topic = Topic::<Subscribe<api::router::Metrics>>::new_static(
        <api::router::Metrics as ContractBody>::TOPIC,
    );
    let subscriber = Subscriber::<api::router::Metrics>::new(&bus, &topic, 32).await?;
    loop {
        let received = subscriber.recv().await?;
        telemetry.record_router(received.body.into());
    }
}

/// Subscribe `v2::joypad::Devices` AND own the `Connect`/`Rescan`
/// publishers for the same tool, driven by the returned command channel - the
/// TUI's Devices tab sends into it (`AppState`'s `DisplayAction::JoypadConnect`/
/// `JoypadRescan`, relayed by `supervise_until_shutdown`), this loop publishes,
/// and the SAME loop iteration's next `Devices` receive is the ack the
/// selection is drawn from (never a local guess - see `JoypadDevicesSample`'s
/// docs). Returns the command sender (installed on `telemetry` immediately,
/// before the feed connects, so a command sent while the bus is still
/// reconnecting is simply queued) plus the feed task's handle.
pub fn start_joypad_devices_feed(
    namespace: String,
    robot_id: String,
    connect: String,
    telemetry: TelemetryBackend,
) -> JoinHandle<()> {
    let (command_tx, mut command_rx) = mpsc::channel(JOYPAD_COMMAND_CHANNEL_CAPACITY);
    telemetry.set_joypad_command_sender(command_tx);
    tokio::spawn(async move {
        loop {
            wait_for_endpoint(&connect).await;
            match joypad_devices_feed_loop(
                namespace.clone(),
                robot_id.clone(),
                connect.clone(),
                &telemetry,
                &mut command_rx,
            )
            .await
            {
                Ok(()) => break,
                Err(error) => {
                    tracing::debug!("joypad devices feed waiting for router: {error:#}");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
        }
    })
}

async fn joypad_devices_feed_loop(
    namespace: String,
    robot_id: String,
    connect: String,
    telemetry: &TelemetryBackend,
    command_rx: &mut mpsc::Receiver<JoypadCommand>,
) -> Result<()> {
    let bus = Bus::open(BusConfig {
        namespace,
        robot_id,
        participant: "phoxal-cli-telemetry-joypad".to_string(),
        incarnation: 0,
        connect_endpoints: vec![connect],
    })
    .await
    .map_err(|error| anyhow!("failed to open bus joypad/devices subscription: {error}"))?;
    let devices_topic = Topic::<Subscribe<api::joypad::Devices>>::new_static(
        <api::joypad::Devices as ContractBody>::TOPIC,
    );
    let devices_subscriber =
        Subscriber::<api::joypad::Devices>::new(&bus, &devices_topic, 32).await?;
    let connect_topic = Topic::<Publish<api::joypad::Connect>>::new_static(
        <api::joypad::Connect as ContractBody>::TOPIC,
    );
    let connect_publisher = Publisher::<api::joypad::Connect>::new(bus.clone(), &connect_topic)?;
    let rescan_topic = Topic::<Publish<api::joypad::Rescan>>::new_static(
        <api::joypad::Rescan as ContractBody>::TOPIC,
    );
    let rescan_publisher = Publisher::<api::joypad::Rescan>::new(bus, &rescan_topic)?;
    loop {
        tokio::select! {
            received = devices_subscriber.recv() => {
                let received = received?;
                telemetry.record_joypad(received.body.into());
            }
            command = command_rx.recv() => {
                match command {
                    Some(JoypadCommand::Connect(id)) => {
                        if let Err(error) = connect_publisher.publish_at(now(), api::joypad::Connect { id }).await {
                            tracing::warn!("joypad connect publish failed: {error:#}");
                        }
                    }
                    Some(JoypadCommand::Rescan) => {
                        if let Err(error) = rescan_publisher.publish_at(now(), api::joypad::Rescan {}).await {
                            tracing::warn!("joypad rescan publish failed: {error:#}");
                        }
                    }
                    // The `TelemetryBackend` handle (and its command sender)
                    // outlives every feed task in practice, so this arm is
                    // untaken outside tests - kept as a clean exit rather
                    // than an unreachable! so a future caller that DOES drop
                    // the backend mid-session degrades gracefully instead of
                    // panicking.
                    None => return Ok(()),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_is_empty_by_default_graceful_absence() {
        let telemetry = TelemetryBackend::new();
        let snapshot = telemetry.snapshot();
        assert!(snapshot.host.is_none());
        assert!(snapshot.clock.is_none());
        assert!(snapshot.router.is_none());
        assert!(snapshot.joypad.is_none());
        assert!(snapshot.process_by_participant.is_empty());
    }

    #[test]
    fn record_host_is_visible_on_the_next_snapshot() {
        let telemetry = TelemetryBackend::new();
        telemetry.record_host(HostSample {
            cpu_pct: 42.0,
            ram_used_bytes: 100,
            ram_total_bytes: 200,
            load_1m: 0.5,
            window_ns: 1_000_000_000,
        });
        let snapshot = telemetry.snapshot();
        assert_eq!(snapshot.host.map(|host| host.value.cpu_pct), Some(42.0));
        assert_eq!(snapshot.host_history.len(), 1);
    }

    /// The bug `TelemetryStore` fixes, exercised through the live
    /// `TelemetryBackend` path rather than the store directly: two
    /// consecutive, IDENTICAL `Host` samples must still advance the
    /// Resources tab's history, since each is a genuinely new receive event
    /// even though the payload happens not to have changed.
    #[test]
    fn record_host_advances_history_on_repeated_identical_samples() {
        let telemetry = TelemetryBackend::new();
        let sample = HostSample {
            cpu_pct: 12.5,
            ram_used_bytes: 100,
            ram_total_bytes: 200,
            load_1m: 0.3,
            window_ns: 1_000_000_000,
        };
        telemetry.record_host(sample);
        telemetry.record_host(sample);
        telemetry.record_host(sample);
        assert_eq!(
            telemetry.snapshot().host_history.len(),
            3,
            "a flat/unchanging live sample must still advance the series"
        );
    }

    #[test]
    fn record_process_demuxes_by_participant_id() {
        let telemetry = TelemetryBackend::new();
        telemetry.record_process(
            "drive".to_string(),
            ProcessSample {
                cpu_pct: 1.0,
                rss_bytes: 10,
                window_ns: 1,
            },
        );
        telemetry.record_process(
            "mission".to_string(),
            ProcessSample {
                cpu_pct: 2.0,
                rss_bytes: 20,
                window_ns: 1,
            },
        );
        let snapshot = telemetry.snapshot();
        assert_eq!(
            snapshot
                .process_by_participant
                .get("drive")
                .map(|p| p.value.cpu_pct),
            Some(1.0)
        );
        assert_eq!(
            snapshot
                .process_by_participant
                .get("mission")
                .map(|p| p.value.cpu_pct),
            Some(2.0)
        );
    }

    #[test]
    fn clock_feed_reads_the_watch_channels_latest_value() {
        let telemetry = TelemetryBackend::new();
        let (tx, rx) = watch::channel(ClockObservation::default());
        telemetry.set_clock_feed(rx);
        assert!(telemetry.snapshot().clock.is_none());
        tx.send_modify(|observation| {
            observation.latest = Some(crate::supervisor::ClockSample { now_ns: 5, step: 3 });
        });
        let sample = telemetry.snapshot().clock.expect("clock sample");
        assert_eq!(sample.step, 3);
    }

    #[test]
    fn joypad_command_without_a_running_feed_is_a_silent_no_op() {
        let telemetry = TelemetryBackend::new();
        // No feed installed a sender yet - must not panic.
        telemetry.send_joypad_command(JoypadCommand::Rescan);
    }
}
