use crate::{
    AttachmentEpoch, InputObservation, ProcessTable, SourceHealth, StoreChanged,
    SupervisorObservation,
};
use std::sync::Arc;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectionObservation {
    Connected,
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
}
