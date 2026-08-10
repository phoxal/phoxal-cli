//! Supervisor control values.

use crate::model::ProcessKey;
use tokio::sync::mpsc;

/// This struct deliberately carries no UI or telemetry handle.
/// Live telemetry (host/process/router/joypad feeds) is owned by the caller
/// and passed directly to
/// the supervisor, and this loop never reads it, so an earlier
/// `telemetry: TelemetryBackend` field
/// here was dead weight, not a real dependency.
#[derive(Debug)]
pub struct SupervisorOptions {
    pub action_rx: Option<mpsc::Receiver<SupervisorAction>>,
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
