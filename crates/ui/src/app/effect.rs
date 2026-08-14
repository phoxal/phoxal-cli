//! Typed commands emitted by the pure UI update function.

use phoxal_cli_observation::{LogRead, RuntimeRead};
use phoxal_runtime_contract::identity::{ParticipantId, ProducerId};
use tokio::sync::mpsc;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceId(pub String);

#[derive(Debug, Clone, PartialEq)]
pub enum Effect {
    Restart {
        process: ParticipantId,
        expected_producer: ProducerId,
    },
    StopProject,
    InputSelect(DeviceId),
    InputEnable(bool),
    InputRescan,
    ReadLogs(LogRead),
    ReadRuntimes(RuntimeRead),
}

#[derive(Debug, Clone)]
pub struct EffectSenders {
    pub guaranteed: mpsc::UnboundedSender<Effect>,
    pub commands: mpsc::Sender<Effect>,
}

/// How an attachment session ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttachmentOutcome {
    /// The client left; the supervisor keeps running. This is what `q` means.
    Detached,
    /// The execution reached a terminal stopped state while attached.
    ExecutionStopped,
    /// The execution failed. `reason` is the supervisor's typed failure when
    /// one reached the client; `None` means the connection was lost with no
    /// failure ever observed (see `update_client`'s `ConnectionChanged`
    /// handling).
    ExecutionFailed {
        reason: Option<phoxal_client::supervisor::execution::SupervisorFailure>,
    },
}
