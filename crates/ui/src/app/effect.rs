//! Typed commands emitted by the pure UI update function.

use phoxal_cli_observation::{LogRead, RuntimeRead};
use tokio::sync::mpsc;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceId(pub String);

#[derive(Debug, Clone, PartialEq)]
pub enum Effect {
    /// End the session this client launched using its mode-specific process
    /// ownership order. There is no restart beside it and no stop that reaches
    /// a remote execution - the supervisor starts nothing, so it stops
    /// nothing, and a client that launched no process has none to end.
    StopSession,
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
    /// The client left with `q`; everything keeps running.
    Detached,
    /// The operator ended the session this client launched, and it is down.
    SessionStopped,
    /// The execution went away while this client was attached.
    ///
    /// `reason` is whatever the client can honestly say: for a session it
    /// launched, its own supervisor's exit; for an attachment to somebody
    /// else's execution, nothing - the supervisor's identity token simply
    /// disappeared, and the caller falls through to the log it points at.
    ExecutionEnded { reason: Option<String> },
}
