//! Typed commands emitted by the pure UI update function.

use phoxal_cli_core::identity::ProducerId;
use phoxal_cli_core::runtime::ProcessKey;
use phoxal_cli_observation::{BusRead, LogRead, RuntimeRead};
use tokio::sync::mpsc;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceId(pub String);

#[derive(Debug, Clone, PartialEq)]
pub enum Effect {
    Restart {
        process: ProcessKey,
        expected_producer: ProducerId,
    },
    StopProject,
    InputSelect(DeviceId),
    InputEnable(bool),
    InputRescan,
    ReadLogs(LogRead),
    ReadBus(BusRead),
    ReadRuntimes(RuntimeRead),
    Detach,
}

#[derive(Debug, Clone)]
pub struct EffectSenders {
    pub guaranteed: mpsc::UnboundedSender<Effect>,
    pub commands: mpsc::Sender<Effect>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachmentOutcome {
    Detached,
    ResidentStopped,
    ResidentFailed,
}
