//! Terminal-neutral telemetry snapshots consumed by session presentation.
//!
//! Bus adapters populate these bounded records; this module has no bus,
//! process, command, or terminal authority.

use std::sync::Arc;
use std::time::Instant;

use phoxal_api::v1 as state_api;

use crate::session::stores::telemetry::Timestamped;

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

#[derive(Debug, Clone, PartialEq)]
pub struct TopicMetric {
    pub topic: String,
    pub from_participant: String,
    pub ingress_rate_hz: f32,
    pub count: u64,
    pub aggregate_overflow: bool,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct RouterMetricsSample {
    pub topics: Arc<Vec<TopicMetric>>,
    pub topics_truncated: u32,
    pub throughput_msg_s: f32,
    pub window_ns: u64,
}

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

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct JoypadDevicesSample {
    pub available: Arc<Vec<JoypadDevice>>,
    pub devices_truncated: usize,
    pub selected: Option<String>,
    pub enabled: bool,
    pub unavailable_reason: Option<String>,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ClockSample {
    pub now_ns: u64,
    pub step: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ClockObservation {
    pub latest: Option<ClockSample>,
    pub received_at: Option<Instant>,
}

#[derive(Debug, Clone, Default)]
pub struct TelemetrySnapshot {
    pub clock: Option<Timestamped<ClockSample>>,
    pub host: Option<Timestamped<HostSample>>,
    pub router: Option<Timestamped<RouterMetricsSample>>,
    pub router_throughput_history: Vec<Timestamped<f32>>,
    pub joypad: Option<Timestamped<JoypadDevicesSample>>,
    pub motion: Option<Timestamped<state_api::motion::State>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JoypadCommand {
    Select(String),
    SetEnabled(bool),
    Rescan,
}
