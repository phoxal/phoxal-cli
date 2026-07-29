use std::collections::BTreeMap;
use std::sync::Arc;

use phoxal_cli_core::runtime::RobotKey;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct DeviceSample {
    pub cpu_pct: Option<f32>,
    pub ram_used_bytes: Option<u64>,
    pub ram_total_bytes: Option<u64>,
    pub swap_used_bytes: Option<u64>,
    pub swap_total_bytes: Option<u64>,
    pub load_1m: Option<f32>,
    pub load_5m: Option<f32>,
    pub load_15m: Option<f32>,
    pub uptime_s: Option<u64>,
    pub disks: Option<Arc<Vec<DeviceDiskSample>>>,
    pub disks_truncated: u32,
    pub window_ns: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeviceDiskSample {
    pub mount_point: String,
    pub file_system: String,
    pub used_bytes: u64,
    pub total_bytes: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ClockSample {
    pub now_ns: u64,
    pub step: u64,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct DeviceObservation {
    pub robots: BTreeMap<RobotKey, DeviceSample>,
    pub clock: Option<ClockSample>,
}
