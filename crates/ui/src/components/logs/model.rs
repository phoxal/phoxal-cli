use std::time::SystemTime;

use phoxal_cli_observation::LogSeverity;
use phoxal_cli_observation::{AttachmentEpoch, LogRow, QueryToken, StoreRevision};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LogSourceFilter {
    #[default]
    All,
    Runtimes,
    /// Everything that is not a robot participant: this CLI's own
    /// diagnostics, the supervisor, and host processes such as Webots.
    System,
}

impl LogSourceFilter {
    pub const fn cycle(self) -> Self {
        match self {
            Self::All => Self::Runtimes,
            Self::Runtimes => Self::System,
            Self::System => Self::All,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SeverityFilter {
    #[default]
    All,
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

impl SeverityFilter {
    pub const fn cycle(self) -> Self {
        match self {
            Self::All => Self::Error,
            Self::Error => Self::Warn,
            Self::Warn => Self::Info,
            Self::Info => Self::Debug,
            Self::Debug => Self::Trace,
            Self::Trace => Self::All,
        }
    }

    pub const fn minimum(self) -> Option<LogSeverity> {
        match self {
            Self::All => None,
            Self::Error => Some(LogSeverity::Error),
            Self::Warn => Some(LogSeverity::Warn),
            Self::Info => Some(LogSeverity::Info),
            Self::Debug => Some(LogSeverity::Debug),
            Self::Trace => Some(LogSeverity::Trace),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Editing {
    Participant,
    Text,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LogsModel {
    pub rows: Vec<LogRow>,
    pub source: LogSourceFilter,
    pub participant: String,
    pub severity: SeverityFilter,
    pub text: String,
    pub control_candidate: usize,
    pub editing: Option<Editing>,
    pub scroll: usize,
    pub follow: bool,
    pub pause_anchor: Option<SystemTime>,
    pub known_revision: StoreRevision,
    pub dirty_revision: Option<StoreRevision>,
    pub in_flight: Option<(AttachmentEpoch, StoreRevision, QueryToken)>,
    pub next_token: u64,
}

impl Default for LogsModel {
    fn default() -> Self {
        Self {
            rows: Vec::new(),
            source: LogSourceFilter::All,
            participant: String::new(),
            severity: SeverityFilter::All,
            text: String::new(),
            control_candidate: 0,
            editing: None,
            scroll: 0,
            follow: true,
            pause_anchor: None,
            known_revision: StoreRevision(0),
            dirty_revision: None,
            in_flight: None,
            next_token: 0,
        }
    }
}
