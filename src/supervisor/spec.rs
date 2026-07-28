//! Participant process specifications and supervision policy.

use super::{ParticipantLaunchCommand, RESTART_SEC, START_LIMIT_BURST, START_LIMIT_INTERVAL};
use phoxal_cli_core::identity::ProducerId;
use phoxal_cli_core::session::ParticipantKind;
use phoxal_cli_core::session::launch_env;
use phoxal_cli_core::session::{
    ParticipantInstanceKey, ProcessKey, ReadinessPolicy, RobotKey, RuntimeFailurePolicy,
    StartupRequirement,
};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::sync::mpsc;

#[derive(Debug, Clone, PartialEq)]
pub struct ParticipantSpec {
    pub key: ProcessKey,
    pub id: String,
    pub kind: ParticipantKind,
    pub executable: PathBuf,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub env: Vec<(String, String)>,
    pub shutdown_grace: Duration,
    pub process_group: bool,
    pub note: Option<String>,
    /// Whether this participant is a checked Phoxal bus participant that
    /// declares its own Zenoh Liveliness token after setup (true for
    /// essentially every spec - services, drivers, and robot tools built on
    /// the shared runner). `false` only for a process with no participant
    /// Liveliness token of its own:
    /// the router (readiness is a separate CLI-owned bus probe) and the Webots
    /// application (process-lifecycle readiness). Drives whether the
    /// supervisor waits for observed presence before marking the process
    /// `Ready`.
    pub bus_participant: bool,
    pub readiness: ReadinessPolicy,
    pub startup_requirement: StartupRequirement,
    pub runtime_failure: RuntimeFailurePolicy,
    pub restart_policy: RestartPolicy,
}

impl ParticipantSpec {
    pub(crate) fn exact_liveliness_template(robot: RobotKey, participant: &str) -> ReadinessPolicy {
        ReadinessPolicy::ExactLiveliness(ParticipantInstanceKey {
            robot,
            participant: participant.to_string(),
            producer: ProducerId::mint(),
        })
    }
    #[must_use]
    pub fn command_line(&self) -> String {
        let mut parts = vec![self.executable.display().to_string()];
        parts.extend(self.args.clone());
        parts.join(" ")
    }

    #[must_use]
    pub fn launch_command(&self) -> ParticipantLaunchCommand {
        ParticipantLaunchCommand {
            command_line: render_manual_command_line(self),
            env: self.env.iter().cloned().collect(),
        }
    }
}

pub(crate) fn render_manual_command_line(spec: &ParticipantSpec) -> String {
    let env = spec.env.iter().cloned().collect::<BTreeMap<_, _>>();
    let mut parts = vec![shell_quote(&spec.executable.display().to_string())];
    parts.extend(spec.args.iter().map(|arg| shell_quote(arg)));
    for (env_key, flag) in launch_env::ENV_TO_FLAG {
        if let Some(value) = env.get(*env_key) {
            parts.push((*flag).to_string());
            parts.push(shell_quote(value));
        }
    }
    parts.join(" ")
}

pub(crate) fn shell_quote(value: &str) -> String {
    if !value.is_empty()
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(byte, b'/' | b'.' | b'-' | b'_' | b':' | b',' | b'=' | b'@')
        })
    {
        return value.to_string();
    }
    let escaped = value.replace('\'', "'\\''");
    format!("'{escaped}'")
}

#[derive(Debug, Clone, PartialEq)]
pub struct RestartPolicy {
    pub restart_delay: Duration,
    pub start_limit_interval: Duration,
    pub start_limit_burst: usize,
}

impl Default for RestartPolicy {
    fn default() -> Self {
        Self {
            restart_delay: RESTART_SEC,
            start_limit_interval: START_LIMIT_INTERVAL,
            start_limit_burst: START_LIMIT_BURST,
        }
    }
}

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
