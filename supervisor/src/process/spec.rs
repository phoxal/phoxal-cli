//! Supervisor control values.

use phoxal_cli_core::runtime::ProcessKey;
use std::sync::Arc;
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
    /// The run's root cancellation signal (the application owns the sender
    /// half): a Ctrl-C observed by the application cancels
    /// this, and this loop selects on it directly instead of its own private
    /// `tokio::signal::ctrl_c()` - the controller is the ONE place that
    /// decides what Ctrl-C means (first = cancel + orderly teardown, second =
    /// force exit), never this loop. Defaults to a fresh, never-cancelled
    /// token so a caller that does not care about cancellation (every test in
    /// this module) does not have to construct one.
    pub token: tokio_util::sync::CancellationToken,
    /// Whether staged startup completion should publish the state owner's
    /// derived Ready or Degraded lifecycle once every stage has spawned and
    /// been observed ready. Simulation clock observation is not a lifecycle
    /// authority.
    pub publishes_running_on_startup_complete: bool,
}

impl Default for SupervisorOptions {
    fn default() -> Self {
        Self {
            action_rx: None,
            token: tokio_util::sync::CancellationToken::new(),
            publishes_running_on_startup_complete: false,
        }
    }
}

#[derive(Debug)]
pub enum SupervisorAction {
    /// Stop and respawn a participant from its own current spec, unchanged -
    /// the TUI's typed `Effect::Restart`. Implemented through
    /// `RunningParticipant::swap` with the participant's own spec cloned back
    /// in, rather than a new field on `RunningParticipant`.
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
