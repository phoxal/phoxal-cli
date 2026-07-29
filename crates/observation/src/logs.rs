use std::time::SystemTime;

use phoxal_cli_core::session::{LogScope, LogSeverity, LogSource};

use crate::{ObservationQuery, ObservationWindow};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum WindowDirection {
    Forward,
    #[default]
    Backward,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LogFilters {
    pub participant: Option<String>,
    pub minimum_severity: Option<LogSeverity>,
    pub scope: Option<LogScope>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum LogAnchor {
    Before(SystemTime),
    After(SystemTime),
    #[default]
    Latest,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogQuery {
    pub filters: LogFilters,
    pub anchor: LogAnchor,
    pub direction: WindowDirection,
    pub limit: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogRow {
    pub participant: String,
    pub source: LogSource,
    pub severity: LogSeverity,
    pub text: String,
    pub event_time: SystemTime,
    pub scope: Option<LogScope>,
}

pub type LogRead = ObservationQuery<LogQuery>;
pub type LogWindow = ObservationWindow<LogRow>;
