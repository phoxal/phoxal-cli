use std::sync::Arc;
use std::time::Instant;

use crate::{ObservationQuery, ObservationWindow, WindowDirection};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct RobotScope {
    pub namespace: String,
    pub robot_id: String,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BusQuery {
    pub topic: Option<String>,
    pub direction: WindowDirection,
    pub limit: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BusRow {
    pub scope: RobotScope,
    pub observed_at: Instant,
    pub topic: String,
    pub participant: String,
    pub rate_hz: f32,
    pub count: u64,
    pub aggregate_overflow: bool,
    pub topics_truncated: u32,
    pub throughput_msg_s: f32,
    pub window_ns: u64,
}

pub type BusRead = ObservationQuery<BusQuery>;
pub type BusWindow = ObservationWindow<BusRow>;
