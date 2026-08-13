//! Supervisor control values.

use crate::model::process::ProcessKey;
use tokio::sync::mpsc;

#[derive(Debug)]
pub(crate) struct SupervisorOptions {
    pub(crate) action_rx: Option<mpsc::Receiver<SupervisorAction>>,
    /// The caller owns cancellation. Once cancelled, supervision stops and
    /// performs orderly teardown; it installs no signal handler of its own.
    pub(crate) token: tokio_util::sync::CancellationToken,
    /// Whether successful startup publishes the daemon-owned `Ready` lifecycle.
    pub(crate) publishes_running_on_startup_complete: bool,
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
pub(crate) enum SupervisorAction {
    /// Stop and respawn one known participant from its current specification.
    Restart { key: ProcessKey },
}
