//! Supervisor control values.

use phoxal_cli_core::session::ProcessKey;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::sync::mpsc;

/// Finding A7/C2: this struct deliberately carries no UI/telemetry handle.
/// Live telemetry (host/process/router/joypad feeds) is owned by the caller
/// and passed directly to
/// the resident supervisor, and this loop never reads it - so an earlier
/// `telemetry: TelemetryBackend` field
/// here was dead weight, not a real dependency.
#[derive(Debug, Clone)]
pub struct SupervisorOptions {
    pub action_rx: Option<SupervisorActionReceiver>,
    pub requested_stop: Option<RequestedStop>,
    /// The session's root cancellation signal (`session::SessionController`
    /// owns the sender half): a Ctrl-C observed by the controller cancels
    /// this, and this loop selects on it directly instead of its own private
    /// `tokio::signal::ctrl_c()` - the controller is the ONE place that
    /// decides what Ctrl-C means (first = cancel + orderly teardown, second =
    /// force exit), never this loop. Defaults to a fresh, never-cancelled
    /// token so a caller that does not care about cancellation (every test in
    /// this module) does not have to construct one.
    pub token: tokio_util::sync::CancellationToken,
    /// Where this loop emits `SessionEvent`s (stage started/finished) for a
    /// live `SessionController` to render - see `phoxal_cli_core::session::event`.
    /// `None` for a caller with no renderer to feed (every test in this
    /// module).
    pub events: Option<mpsc::Sender<phoxal_cli_core::session::event::SessionEvent>>,
    /// Whether staged startup completion should transition the session to
    /// `SessionState::Running`. When `true`, once
    /// every stage has spawned and been observed ready with nothing left
    /// pending, the state owner derives and publishes Ready or Degraded and
    /// emits the corresponding session lifecycle change. Simulation clock
    /// telemetry is not a lifecycle authority.
    pub emits_running_on_startup_complete: bool,
}

impl Default for SupervisorOptions {
    fn default() -> Self {
        Self {
            action_rx: None,
            requested_stop: None,
            token: tokio_util::sync::CancellationToken::new(),
            events: None,
            emits_running_on_startup_complete: false,
        }
    }
}

#[derive(Debug, Clone)]
pub struct RequestedStop {
    pub(crate) key: ProcessKey,
    pub(crate) grace: Duration,
}

impl RequestedStop {
    pub fn new(key: impl Into<ProcessKey>, grace: Duration) -> Self {
        Self {
            key: key.into(),
            grace,
        }
    }
}

#[derive(Debug)]
pub enum SupervisorAction {
    /// Stop and respawn a participant from its own current spec, unchanged -
    /// the TUI's `r restart` (see `crate::display::DisplayAction::Restart`).
    /// Handled the same way as `Swap` with the participant's own spec cloned
    /// back in, rather than a new field on `RunningParticipant`, so it reuses
    /// the exact same stop/spawn/board-note sequence a hot-reload swap
    /// already goes through.
    Restart { key: ProcessKey },
}

#[derive(Clone)]
pub struct SupervisorActionReceiver {
    inner: Arc<Mutex<Option<mpsc::Receiver<SupervisorAction>>>>,
}

impl std::fmt::Debug for SupervisorActionReceiver {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("SupervisorActionReceiver(..)")
    }
}

impl SupervisorActionReceiver {
    #[must_use]
    pub fn new(receiver: mpsc::Receiver<SupervisorAction>) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Some(receiver))),
        }
    }

    pub(crate) async fn recv(&self) -> Option<SupervisorAction> {
        let mut receiver = self.inner.lock().await;
        let Some(active) = receiver.as_mut() else {
            drop(receiver);
            return std::future::pending().await;
        };
        let action = active.recv().await;
        if action.is_none() {
            receiver.take();
        }
        action
    }
}
