use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use crate::{
    AttachmentEpoch, InputObservation, ProcessTable, SourceHealth, StoreChanged,
    SupervisorObservation,
};
pub use phoxal_cli_core::runtime::{PhaseId, PhaseOutcome};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Freshness {
    Fresh,
    Stale,
}

pub type FreshnessSet = BTreeMap<String, Freshness>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectionObservation {
    Connected,
    Reconnecting { attempt: u32 },
    Terminal,
    Lost { reason: Arc<str> },
}

#[derive(Debug, Clone, PartialEq)]
pub enum AttachmentEvent {
    EpochChanged(AttachmentEpoch),
    ConnectionChanged(ConnectionObservation),
    SupervisorChanged(Arc<SupervisorObservation>),
    ProcessesChanged {
        epoch: AttachmentEpoch,
        values: Arc<ProcessTable>,
    },
    InputChanged {
        epoch: AttachmentEpoch,
        values: Arc<InputObservation>,
    },
    SourceHealthChanged {
        epoch: AttachmentEpoch,
        values: Arc<SourceHealth>,
    },
    LogsChanged(StoreChanged),
    RuntimesChanged(StoreChanged),
    FreshnessChanged {
        epoch: AttachmentEpoch,
        values: FreshnessSet,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiagnosticSource {
    Tracing,
    Cli,
    Dependency,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum DiagnosticLevel {
    Info,
    Warn,
    Error,
}

/// Bounded messages produced by project preparation and supervision for a UI.
#[derive(Debug, Clone)]
pub enum RuntimeEvent {
    PhaseStarted {
        id: PhaseId,
        label: String,
    },
    PhaseFinished {
        id: PhaseId,
        outcome: PhaseOutcome,
        elapsed: Duration,
    },
    Diagnostic {
        source: DiagnosticSource,
        level: DiagnosticLevel,
        message: String,
    },
}
