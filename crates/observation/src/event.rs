use std::collections::BTreeMap;
use std::sync::Arc;

use crate::{
    AttachmentEpoch, DeviceObservation, InputObservation, ProcessTable, SourceHealth, StoreChanged,
    SupervisorObservation,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Freshness {
    Fresh,
    Stale,
}

pub type FreshnessSet = BTreeMap<String, Freshness>;

#[derive(Debug, Clone, PartialEq)]
pub enum AttachmentEvent {
    EpochChanged(AttachmentEpoch),
    SupervisorChanged(Arc<SupervisorObservation>),
    ProcessesChanged(Arc<ProcessTable>),
    DeviceChanged(Arc<DeviceObservation>),
    InputChanged(Arc<InputObservation>),
    SourceHealthChanged(Arc<SourceHealth>),
    LogsChanged(StoreChanged),
    BusChanged(StoreChanged),
    RuntimesChanged(StoreChanged),
    FreshnessChanged(FreshnessSet),
}
