//! Live telemetry feed for the TUI (CLI-UX Phase 3/4): background bus
//! subscribers for the framework's `v2` router/host-telemetry/joypad
//! contracts, plus the simulation clock's live step/time readout,
//! mirroring `supervisor::start_bus_log_subscriber`/
//! `start_presence_heartbeat_subscriber`'s "subscribe, update a shared
//! snapshot" pattern.
//!
//! Kept deliberately separate from `supervisor::BoardBackend`/`BoardSnapshot`
//! (participant board state, persisted to the state file): telemetry never
//! reaches either of those, only the live TUI reads it
//! (`TelemetryBackend::snapshot`), so it can carry
//! whatever shape is convenient for rendering without touching the persisted
//! board contract.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use phoxal::bus::{ContractBody, Publish, Publisher, Subscribe, Subscriber, Topic};
use phoxal::raw::{Bus, BusConfig, host_time};
use phoxal_api::{v1 as state_api, v2 as api};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

use crate::stores::log_store::sanitize_terminal_text;
use crate::stores::telemetry_store::{TelemetryStore, Timestamped};
use crate::supervisor::{ClockObservation, wait_for_endpoint};

const MAX_HOST_DISKS: usize = 32;
const MAX_ROUTER_TOPICS: usize = 256;
const MAX_JOYPAD_DEVICES: usize = 64;
const MAX_REMOTE_TEXT_CHARS: usize = 256;

/// One `telemetry::Host` sample (the host resource meter).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct HostSample {
    pub cpu_pct: f32,
    pub ram_used_bytes: u64,
    pub ram_total_bytes: u64,
    pub swap_used_bytes: u64,
    pub swap_total_bytes: u64,
    pub load_1m: f32,
    pub load_5m: f32,
    pub load_15m: f32,
    pub uptime_s: Option<u64>,
    pub disks: Arc<Vec<DiskSample>>,
    pub disks_truncated: u32,
    pub window_ns: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DiskSample {
    pub mount_point: String,
    pub file_system: String,
    pub used_bytes: u64,
    pub total_bytes: u64,
}

impl From<api::telemetry::Host> for HostSample {
    fn from(body: api::telemetry::Host) -> Self {
        let wire_truncated = body
            .disks
            .iter()
            .filter(|disk| disk.mount_point.is_empty())
            .filter_map(|disk| {
                disk.file_system
                    .strip_prefix('+')
                    .and_then(|value| value.strip_suffix(" omitted"))
                    .and_then(|value| value.parse::<u32>().ok())
            })
            .fold(0_u32, u32::saturating_add);
        let received_disks = body
            .disks
            .iter()
            .filter(|disk| !disk.mount_point.is_empty())
            .count();
        let disks = body
            .disks
            .into_iter()
            .filter(|disk| !disk.mount_point.is_empty())
            .take(MAX_HOST_DISKS)
            .map(|disk| DiskSample {
                mount_point: bounded_remote_text(&disk.mount_point),
                file_system: bounded_remote_text(&disk.file_system),
                used_bytes: disk.used_bytes,
                total_bytes: disk.total_bytes,
            })
            .collect::<Vec<_>>();
        let locally_truncated = received_disks.saturating_sub(disks.len());
        Self {
            cpu_pct: body.cpu_pct,
            ram_used_bytes: body.ram_used_bytes,
            ram_total_bytes: body.ram_total_bytes,
            swap_used_bytes: body.swap_used_bytes,
            swap_total_bytes: body.swap_total_bytes,
            load_1m: body.load_1m,
            load_5m: body.load_5m,
            load_15m: body.load_15m,
            uptime_s: body.uptime_s,
            disks: Arc::new(disks),
            disks_truncated: wire_truncated
                .saturating_add(u32::try_from(locally_truncated).unwrap_or(u32::MAX)),
            window_ns: body.window_ns,
        }
    }
}

/// One row of the global Bus page.
#[derive(Debug, Clone, PartialEq)]
pub struct TopicMetric {
    pub topic: String,
    pub from_participant: String,
    pub ingress_rate_hz: f32,
    pub count: u64,
    /// The framework could not attribute every observed message to a detailed
    /// row, either because the table was capped or its non-blocking mirror
    /// queue dropped a burst. It may combine robot and internal traffic and
    /// must not be presented as a real producer.
    pub aggregate_overflow: bool,
}

impl From<api::router::TopicMetric> for TopicMetric {
    fn from(body: api::router::TopicMetric) -> Self {
        let aggregate_overflow = body.topic.is_empty() && body.from_participant.is_empty();
        Self {
            topic: if aggregate_overflow {
                "Other/unobserved traffic".to_string()
            } else {
                bounded_remote_text(&body.topic)
            },
            from_participant: if aggregate_overflow {
                "multiple".to_string()
            } else {
                bounded_remote_text(&body.from_participant)
            },
            ingress_rate_hz: body.ingress_rate_hz,
            count: body.count,
            aggregate_overflow,
        }
    }
}

/// The router's latest `router::Metrics` sample.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RouterMetricsSample {
    pub topics: Arc<Vec<TopicMetric>>,
    pub topics_truncated: u32,
    pub throughput_msg_s: f32,
    pub window_ns: u64,
}

impl From<api::router::Metrics> for RouterMetricsSample {
    fn from(body: api::router::Metrics) -> Self {
        let received_topics = body.topics.len();
        let keep = if received_topics > MAX_ROUTER_TOPICS {
            MAX_ROUTER_TOPICS.saturating_sub(1)
        } else {
            MAX_ROUTER_TOPICS
        };
        let mut wire_topics = body.topics;
        let dropped = if wire_topics.len() > keep {
            wire_topics.split_off(keep)
        } else {
            Vec::new()
        };
        let dropped_rate = dropped.iter().map(|metric| metric.ingress_rate_hz).sum();
        let dropped_count = dropped
            .iter()
            .fold(0_u64, |total, metric| total.saturating_add(metric.count));
        let mut topics = wire_topics
            .into_iter()
            .map(TopicMetric::from)
            .collect::<Vec<_>>();
        if received_topics > keep {
            if let Some(overflow) = topics.iter_mut().find(|metric| metric.aggregate_overflow) {
                overflow.ingress_rate_hz += dropped_rate;
                overflow.count = overflow.count.saturating_add(dropped_count);
            } else {
                topics.push(TopicMetric {
                    topic: "Other/unobserved traffic".to_string(),
                    from_participant: "multiple".to_string(),
                    ingress_rate_hz: dropped_rate,
                    count: dropped_count,
                    aggregate_overflow: true,
                });
            }
        }
        Self {
            topics: Arc::new(topics),
            topics_truncated: u32::try_from(received_topics.saturating_sub(keep))
                .unwrap_or(u32::MAX),
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
    pub status: JoypadDeviceStatus,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum JoypadDeviceStatus {
    Ready,
    Disconnected,
    Unsupported,
    #[default]
    Unknown,
}

impl From<api::joypad::Device> for JoypadDevice {
    fn from(body: api::joypad::Device) -> Self {
        Self {
            id: bounded_remote_text(&body.id),
            name: bounded_remote_text(&body.name),
            status: match body.status {
                api::joypad::DeviceStatus::Ready => JoypadDeviceStatus::Ready,
                api::joypad::DeviceStatus::Disconnected => JoypadDeviceStatus::Disconnected,
                api::joypad::DeviceStatus::Unsupported => JoypadDeviceStatus::Unsupported,
            },
        }
    }
}

/// The joypad tool's latest published device state - `selected` is the
/// AUTHORITATIVE selection (the tool's own ack), never a local UI guess; see
/// `tui::state::AppState::input_cursor` for the separate, purely local list cursor.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct JoypadDevicesSample {
    pub available: Arc<Vec<JoypadDevice>>,
    pub devices_truncated: usize,
    pub selected: Option<String>,
    pub enabled: bool,
    pub unavailable_reason: Option<String>,
    pub last_error: Option<String>,
}

impl From<api::joypad::Devices> for JoypadDevicesSample {
    fn from(body: api::joypad::Devices) -> Self {
        let received_devices = body.available.len();
        let available = body
            .available
            .into_iter()
            .take(MAX_JOYPAD_DEVICES)
            .map(JoypadDevice::from)
            .collect::<Vec<_>>();
        Self {
            devices_truncated: received_devices.saturating_sub(available.len()),
            available: Arc::new(available),
            selected: body.selected.map(|id| bounded_remote_text(&id)),
            enabled: body.enabled,
            unavailable_reason: body
                .unavailable_reason
                .map(|reason| bounded_remote_text(&reason)),
            last_error: body.last_error.map(|error| bounded_remote_text(&error)),
        }
    }
}

fn bounded_remote_text(text: &str) -> String {
    sanitize_terminal_text(text)
        .chars()
        .take(MAX_REMOTE_TEXT_CHARS)
        .collect()
}

/// A snapshot of every live telemetry feed, cloned once per TUI redraw
/// (mirrors `BoardBackend::snapshot`). Every latest-value field is a
/// [`Timestamped`] carrying the [`Instant`] the underlying sample was
/// actually RECEIVED off the bus (recorded by `TelemetryBackend::record_*`
/// at the moment a feed task observes it, never re-stamped on later
/// redraws), so a renderer can ask [`Timestamped::is_stale`] instead of
/// trusting a long-cached value as if it were still live. See the
/// `stores::telemetry_store` module documentation for the store contract.
#[derive(Debug, Clone, Default)]
pub struct TelemetrySnapshot {
    /// The simulation clock's latest observed sample - `None` in `run` mode
    /// (no clock feed is ever started there) or before the first sample
    /// arrives in `simulation` mode.
    pub clock: Option<Timestamped<crate::supervisor::ClockSample>>,
    /// `None` until `tool-telemetry`'s first `Host` sample arrives - this is
    /// the graceful-absence case (the tool may not be in the catalog yet),
    /// rendered as `cpu n/a` rather than a failure.
    pub host: Option<Timestamped<HostSample>>,
    pub router: Option<Timestamped<RouterMetricsSample>>,
    /// Receive-time history of the 60 most recent total-throughput samples,
    /// bounded by `TelemetryStore` for the live Bus sparkline.
    pub router_throughput_history: Vec<Timestamped<f32>>,
    pub joypad: Option<Timestamped<JoypadDevicesSample>>,
    pub motion: Option<Timestamped<state_api::motion::State>>,
}

/// A joypad action the global Input page asked to publish.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JoypadCommand {
    Select(String),
    SetEnabled(bool),
    Rescan,
}

/// Live-telemetry state shared between the background feed tasks
/// (`start_host_feed` etc.) and the TUI's redraw path
/// (`TuiDisplay::redraw`/`render::draw`). Cheap to clone (an `Arc` handle);
/// every feed task and the TUI hold their own clone.
///
/// Backed by [`TelemetryStore`]: every `record_*` call below
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
/// keypress in Input, never a hot loop): [`JoypadCommand`]s queue
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

    /// Publish a joypad Select, SetEnabled, or Rescan command from the TUI's
    /// Input page. Never blocks the terminal input path. An absent, closed, or
    /// overloaded feed is reported through tracing so the failure appears in
    /// Logs instead of being duplicated as persistent Input-panel state.
    pub fn send_joypad_command(&self, command: JoypadCommand) {
        let sender = self
            .joypad_command_tx
            .lock()
            .expect("joypad_command_tx mutex poisoned")
            .clone();
        let Some(sender) = sender else {
            tracing::warn!(?command, "joypad action rejected: tool feed is unavailable");
            return;
        };
        match sender.try_send(command) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(command)) => {
                tracing::warn!(?command, "joypad action rejected: tool feed is busy");
            }
            Err(mpsc::error::TrySendError::Closed(command)) => {
                tracing::warn!(?command, "joypad action rejected: tool feed stopped");
            }
        }
    }

    #[must_use]
    pub fn snapshot(&self) -> TelemetrySnapshot {
        let store = self.inner.lock().expect("telemetry mutex poisoned");
        let mut snapshot = TelemetrySnapshot {
            clock: None,
            host: store.host().cloned(),
            router: store.router().cloned(),
            router_throughput_history: store.router_throughput_history().iter().copied().collect(),
            joypad: store.joypad().cloned(),
            motion: store.motion().cloned(),
        };
        drop(store);
        if let Some(rx) = &*self.clock_rx.lock().expect("clock_rx mutex poisoned") {
            let observation = rx.borrow();
            snapshot.clock = observation
                .latest
                .zip(observation.received_at)
                .map(|(value, received_at)| Timestamped { value, received_at });
        }
        snapshot
    }

    fn record_host(&self, sample: HostSample) {
        self.inner
            .lock()
            .expect("telemetry mutex poisoned")
            .record_host(Instant::now(), sample);
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
    let result = async {
        let motion_topic = Topic::<Subscribe<state_api::motion::State>>::new_static(
            <state_api::motion::State as ContractBody>::TOPIC,
        );
        let motion = Subscriber::new(&bus, &motion_topic, 32).await?;
        loop {
            telemetry.record_motion(motion.recv().await?.body);
        }
    }
    .await;
    close_feed_bus(&bus, "control-state").await;
    result
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
    let result = async {
        let topic = Topic::<Subscribe<api::telemetry::Host>>::new_static(
            <api::telemetry::Host as ContractBody>::TOPIC,
        );
        let subscriber = Subscriber::<api::telemetry::Host>::new(&bus, &topic, 32).await?;
        loop {
            let received = subscriber.recv().await?;
            telemetry.record_host(received.body.into());
        }
    }
    .await;
    close_feed_bus(&bus, "telemetry/host").await;
    result
}

/// Subscribe v2::router::Metrics for the global Bus page.
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
    let result = async {
        let topic = Topic::<Subscribe<api::router::Metrics>>::new_static(
            <api::router::Metrics as ContractBody>::TOPIC,
        );
        let subscriber = Subscriber::<api::router::Metrics>::new(&bus, &topic, 32).await?;
        loop {
            let received = subscriber.recv().await?;
            telemetry.record_router(received.body.into());
        }
    }
    .await;
    close_feed_bus(&bus, "router/metrics").await;
    result
}

/// Subscribe v2::joypad::Devices and own the Select, SetEnabled, and Rescan
/// publishers. The TUI's Input page sends commands through DisplayAction;
/// this loop publishes, and the next Devices receive is the authoritative
/// acknowledgement. The command sender is installed on telemetry immediately,
/// before the feed connects, so a command sent while the bus reconnects is
/// queued. The returned handle owns both subscription and command publishing.
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
    let result = async {
        let devices_topic = Topic::<Subscribe<api::joypad::Devices>>::new_static(
            <api::joypad::Devices as ContractBody>::TOPIC,
        );
        let devices_subscriber =
            Subscriber::<api::joypad::Devices>::new(&bus, &devices_topic, 32).await?;
        let select_topic = Topic::<Publish<api::joypad::Select>>::new_static(
            <api::joypad::Select as ContractBody>::TOPIC,
        );
        let select_publisher = Publisher::<api::joypad::Select>::new(bus.clone(), &select_topic)?;
        let enabled_topic = Topic::<Publish<api::joypad::SetEnabled>>::new_static(
            <api::joypad::SetEnabled as ContractBody>::TOPIC,
        );
        let enabled_publisher =
            Publisher::<api::joypad::SetEnabled>::new(bus.clone(), &enabled_topic)?;
        let rescan_topic = Topic::<Publish<api::joypad::Rescan>>::new_static(
            <api::joypad::Rescan as ContractBody>::TOPIC,
        );
        let rescan_publisher = Publisher::<api::joypad::Rescan>::new(bus.clone(), &rescan_topic)?;
        loop {
            tokio::select! {
                received = devices_subscriber.recv() => {
                    let received = received?;
                    telemetry.record_joypad(received.body.into());
                }
                command = command_rx.recv() => {
                    match command {
                        Some(JoypadCommand::Select(id)) => {
                            if let Err(error) = select_publisher.publish_at(host_time(), api::joypad::Select { id }).await {
                                tracing::warn!("joypad select publish failed: {error:#}");
                            }
                        }
                        Some(JoypadCommand::SetEnabled(enabled)) => {
                            if let Err(error) = enabled_publisher.publish_at(host_time(), api::joypad::SetEnabled { enabled }).await {
                                tracing::warn!("joypad enable publish failed: {error:#}");
                            }
                        }
                        Some(JoypadCommand::Rescan) => {
                            if let Err(error) = rescan_publisher.publish_at(host_time(), api::joypad::Rescan {}).await {
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
    .await;
    close_feed_bus(&bus, "joypad/devices").await;
    result
}

async fn close_feed_bus(bus: &Bus, feed: &str) {
    if let Err(error) = bus.close().await {
        tracing::debug!(feed, error = %error, "telemetry feed bus close failed");
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
            ..HostSample::default()
        });
        let snapshot = telemetry.snapshot();
        assert_eq!(snapshot.host.map(|host| host.value.cpu_pct), Some(42.0));
    }

    #[test]
    fn router_overflow_sentinel_is_presented_as_an_aggregate_row() {
        let metric = TopicMetric::from(api::router::TopicMetric {
            topic: String::new(),
            from_participant: String::new(),
            ingress_rate_hz: 7.0,
            count: 11,
        });
        assert_eq!(metric.topic, "Other/unobserved traffic");
        assert_eq!(metric.from_participant, "multiple");
        assert_eq!(metric.ingress_rate_hz, 7.0);
        assert_eq!(metric.count, 11);
        assert!(metric.aggregate_overflow);
    }

    #[test]
    fn remote_router_and_joypad_labels_are_sanitized_at_ingress() {
        let metric = TopicMetric::from(api::router::TopicMetric {
            topic: "v1/drive\u{1b}[31m/state".to_string(),
            from_participant: "drive\nspoof".to_string(),
            ingress_rate_hz: 1.0,
            count: 1,
        });
        assert_eq!(metric.topic, "v1/drive/state");
        assert_eq!(metric.from_participant, "drive spoof");

        let devices = JoypadDevicesSample::from(api::joypad::Devices {
            available: vec![api::joypad::Device {
                id: "pad\u{1b}[2J".to_string(),
                name: "Pad\nname".to_string(),
                status: api::joypad::DeviceStatus::Ready,
            }],
            selected: Some("pad\u{1b}[2J".to_string()),
            enabled: false,
            unavailable_reason: Some("reason\rline".to_string()),
            last_error: Some("error\tline".to_string()),
        });
        assert_eq!(devices.available[0].id, "pad");
        assert_eq!(devices.available[0].name, "Pad name");
        assert_eq!(devices.selected.as_deref(), Some("pad"));
        assert_eq!(devices.unavailable_reason.as_deref(), Some("reason line"));
        assert_eq!(devices.last_error.as_deref(), Some("error line"));
    }

    #[test]
    fn oversized_remote_telemetry_is_bounded_and_disclosed() {
        let router = RouterMetricsSample::from(api::router::Metrics {
            topics: (0..(MAX_ROUTER_TOPICS + 20))
                .map(|index| api::router::TopicMetric {
                    topic: format!("{}-{index}", "t".repeat(MAX_REMOTE_TEXT_CHARS * 2)),
                    from_participant: "p".repeat(MAX_REMOTE_TEXT_CHARS * 2),
                    ingress_rate_hz: 1.0,
                    count: 1,
                })
                .collect(),
            throughput_msg_s: 1.0,
            window_ns: 1,
        });
        assert_eq!(router.topics.len(), MAX_ROUTER_TOPICS);
        assert_eq!(router.topics_truncated, 21);
        let overflow = router
            .topics
            .iter()
            .find(|metric| metric.aggregate_overflow)
            .expect("overflow aggregate");
        assert_eq!(overflow.ingress_rate_hz, 21.0);
        assert_eq!(overflow.count, 21);
        assert!(router.topics.iter().any(|metric| metric.aggregate_overflow));
        assert!(router.topics.iter().all(|metric| {
            metric.topic.chars().count() <= MAX_REMOTE_TEXT_CHARS
                && metric.from_participant.chars().count() <= MAX_REMOTE_TEXT_CHARS
        }));

        let joypad = JoypadDevicesSample::from(api::joypad::Devices {
            available: (0..(MAX_JOYPAD_DEVICES + 7))
                .map(|index| api::joypad::Device {
                    id: format!("{}-{index}", "i".repeat(MAX_REMOTE_TEXT_CHARS * 2)),
                    name: "n".repeat(MAX_REMOTE_TEXT_CHARS * 2),
                    status: api::joypad::DeviceStatus::Ready,
                })
                .collect(),
            selected: None,
            enabled: false,
            unavailable_reason: None,
            last_error: None,
        });
        assert_eq!(joypad.available.len(), MAX_JOYPAD_DEVICES);
        assert_eq!(joypad.devices_truncated, 7);

        let host = HostSample::from(api::telemetry::Host {
            cpu_pct: 0.0,
            ram_used_bytes: 0,
            ram_total_bytes: 0,
            swap_used_bytes: 0,
            swap_total_bytes: 0,
            load_1m: 0.0,
            load_5m: 0.0,
            load_15m: 0.0,
            uptime_s: None,
            disks: (0..(MAX_HOST_DISKS + 4))
                .map(|index| api::telemetry::Disk {
                    mount_point: format!("/disk-{index}"),
                    file_system: "fs".to_string(),
                    used_bytes: 0,
                    total_bytes: 1,
                })
                .chain(std::iter::once(api::telemetry::Disk {
                    mount_point: String::new(),
                    file_system: "+3 omitted".to_string(),
                    used_bytes: 0,
                    total_bytes: 0,
                }))
                .collect(),
            window_ns: 1,
        });
        assert_eq!(host.disks.len(), MAX_HOST_DISKS);
        assert_eq!(host.disks_truncated, 7);
    }

    #[test]
    fn local_router_truncation_folds_into_an_existing_overflow_sentinel() {
        let mut topics = vec![api::router::TopicMetric {
            topic: String::new(),
            from_participant: String::new(),
            ingress_rate_hz: 100.0,
            count: 100,
        }];
        topics.extend(
            (0..(MAX_ROUTER_TOPICS + 20)).map(|index| api::router::TopicMetric {
                topic: format!("v1/topic/{index}"),
                from_participant: format!("producer-{index}"),
                ingress_rate_hz: 1.0,
                count: 1,
            }),
        );

        let router = RouterMetricsSample::from(api::router::Metrics {
            topics,
            throughput_msg_s: 0.0,
            window_ns: 1,
        });
        let overflow = router
            .topics
            .iter()
            .find(|metric| metric.aggregate_overflow)
            .expect("router sentinel must remain visible");
        assert_eq!(router.topics_truncated, 22);
        assert_eq!(overflow.ingress_rate_hz, 122.0);
        assert_eq!(overflow.count, 122);
    }

    #[test]
    fn telemetry_snapshots_share_large_latest_value_storage() {
        let telemetry = TelemetryBackend::new();
        telemetry.record_router(RouterMetricsSample {
            topics: Arc::new(vec![TopicMetric {
                topic: "v1/motion/state".to_string(),
                from_participant: "motion".to_string(),
                ingress_rate_hz: 1.0,
                count: 1,
                aggregate_overflow: false,
            }]),
            ..RouterMetricsSample::default()
        });
        let first = telemetry.snapshot().router.expect("first router sample");
        let second = telemetry.snapshot().router.expect("second router sample");
        assert!(Arc::ptr_eq(&first.value.topics, &second.value.topics));
    }

    #[test]
    fn clock_feed_reads_the_watch_channels_latest_value() {
        let telemetry = TelemetryBackend::new();
        let (tx, rx) = watch::channel(ClockObservation::default());
        telemetry.set_clock_feed(rx);
        assert!(telemetry.snapshot().clock.is_none());
        tx.send_modify(|observation| {
            observation.latest = Some(crate::supervisor::ClockSample { now_ns: 5, step: 3 });
            observation.received_at = Some(Instant::now());
        });
        let sample = telemetry.snapshot().clock.expect("clock sample");
        assert_eq!(sample.value.step, 3);
    }

    #[test]
    fn joypad_command_without_a_running_feed_does_not_panic() {
        let telemetry = TelemetryBackend::new();
        // No feed installed a sender yet - the rejected action is logged and
        // must not panic or block the input path.
        telemetry.send_joypad_command(JoypadCommand::Rescan);
    }
}
