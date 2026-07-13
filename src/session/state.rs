//! The pure session state machine a later `SessionController` slice will
//! drive from the supervisor and preparation lifecycle.
//!
//! Every transition is a method that consumes `self` and returns the next
//! state or a documented [`InvalidTransition`] error - illegal edges are
//! representable as data, never a panic, so a caller (or a test) can assert
//! on them directly.
//!
//! ```text
//! Preparing -> Starting -> Waiting | Paused | Running -> Stopping -> Stopped
//!                                        \-> Failed
//! ```
//!
//! `Stopped` and `Failed` are terminal: no transition leaves them. `Stopping`
//! only ever resolves to `Stopped` - it is not itself a valid source for
//! `Waiting`/`Paused`/`Running`/`Failed`, matching the diagram's single
//! `Stopping -> Stopped` edge. Every other non-terminal state additionally
//! accepts `Stopping` (Ctrl-C) and `Failed` (a terminal failure), which is
//! what lets `Preparing` fail or be cancelled before `Starting` ever begins.

use std::fmt;

/// A structured reason the session is waiting rather than running.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WaitReason {
    /// Waiting on a named participant to become ready.
    Participant(String),
    /// The simulation clock has not published a first sample yet.
    ClockAbsent,
    /// Waiting on a named standard tool's connection (router, joypad, ...).
    ToolConnection(String),
}

/// Why the session ended in `Failed`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FailReason {
    /// A named participant failed in a way the session cannot recover from.
    Participant(String),
    /// A bounded wait exceeded its explicit, non-interactive policy.
    Timeout,
    /// Any other terminal failure, carrying a short human message.
    Terminal(String),
}

/// Evidence that a clock sample was actually observed, required to enter
/// [`SessionState::Paused`].
///
/// `Paused` must be reachable only after an observed clock sample with
/// `running: false` (a published paused clock is a healthy, explicit state -
/// never inferred from silence). Carrying that evidence as a transition
/// argument, rather than trusting the caller's choice of method, keeps the
/// invariant enforced by the type rather than by convention.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClockObservation {
    pub running: bool,
}

/// The session's lifecycle state. See the module docs for the full
/// transition diagram and invariants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionState {
    Preparing,
    Starting,
    Waiting(WaitReason),
    Paused,
    Running,
    Stopping,
    Stopped,
    Failed(FailReason),
}

/// One rejected transition attempt: the state it was attempted from, the
/// transition that was attempted, and - for invariant violations rather than
/// a missing edge - why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidTransition {
    from: &'static str,
    attempted: &'static str,
    detail: Option<&'static str>,
}

impl fmt::Display for InvalidTransition {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "invalid session transition: {} -> {}",
            self.from, self.attempted
        )?;
        if let Some(detail) = self.detail {
            write!(formatter, " ({detail})")?;
        }
        Ok(())
    }
}

impl std::error::Error for InvalidTransition {}

impl SessionState {
    fn label_str(&self) -> &'static str {
        match self {
            Self::Preparing => "preparing",
            Self::Starting => "starting",
            Self::Waiting(_) => "waiting",
            Self::Paused => "paused",
            Self::Running => "running",
            Self::Stopping => "stopping",
            Self::Stopped => "stopped",
            Self::Failed(_) => "failed",
        }
    }

    fn invalid(&self, attempted: &'static str, detail: Option<&'static str>) -> InvalidTransition {
        InvalidTransition {
            from: self.label_str(),
            attempted,
            detail,
        }
    }

    /// A short human label for the runtime console, e.g. `"paused"` or
    /// `"waiting for simulation clock"`.
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            Self::Waiting(reason) => format!("waiting for {}", wait_reason_label(reason)),
            Self::Failed(reason) => format!("failed: {}", fail_reason_label(reason)),
            _ => self.label_str().to_string(),
        }
    }

    /// `true` once the session has reached a state no transition leaves.
    #[must_use]
    pub const fn is_terminal(&self) -> bool {
        matches!(self, Self::Stopped | Self::Failed(_))
    }

    /// `true` while the session's lifecycle is still progressing, i.e. the
    /// complement of [`Self::is_terminal`].
    #[must_use]
    pub const fn is_active(&self) -> bool {
        !self.is_terminal()
    }

    /// `Preparing -> Starting`.
    pub fn start(self) -> Result<Self, InvalidTransition> {
        match self {
            Self::Preparing => Ok(Self::Starting),
            other => Err(other.invalid("starting", None)),
        }
    }

    /// `{Starting, Paused, Running} -> Waiting(reason)`.
    pub fn to_waiting(self, reason: WaitReason) -> Result<Self, InvalidTransition> {
        match self {
            Self::Starting | Self::Paused | Self::Running => Ok(Self::Waiting(reason)),
            other => Err(other.invalid("waiting", None)),
        }
    }

    /// `{Starting, Waiting, Running} -> Paused`.
    ///
    /// Requires `observation.running == false`: `Paused` is reachable only
    /// after an observed clock sample proving the clock is genuinely
    /// paused, never inferred from silence.
    pub fn to_paused(self, observation: ClockObservation) -> Result<Self, InvalidTransition> {
        match self {
            Self::Starting | Self::Waiting(_) | Self::Running => {
                if observation.running {
                    Err(self.invalid(
                        "paused",
                        Some("paused requires an observed clock sample with running: false"),
                    ))
                } else {
                    Ok(Self::Paused)
                }
            }
            other => Err(other.invalid("paused", None)),
        }
    }

    /// `{Starting, Waiting, Paused} -> Running`.
    pub fn to_running(self) -> Result<Self, InvalidTransition> {
        match self {
            Self::Starting | Self::Waiting(_) | Self::Paused => Ok(Self::Running),
            other => Err(other.invalid("running", None)),
        }
    }

    /// Any non-terminal state except `Stopping` itself `-> Stopping`
    /// (Ctrl-C / a graceful stop request).
    pub fn to_stopping(self) -> Result<Self, InvalidTransition> {
        match self {
            Self::Preparing | Self::Starting | Self::Waiting(_) | Self::Paused | Self::Running => {
                Ok(Self::Stopping)
            }
            other => Err(other.invalid("stopping", None)),
        }
    }

    /// `Stopping -> Stopped`.
    pub fn to_stopped(self) -> Result<Self, InvalidTransition> {
        match self {
            Self::Stopping => Ok(Self::Stopped),
            other => Err(other.invalid("stopped", None)),
        }
    }

    /// Any non-terminal state except `Stopping` itself `-> Failed(reason)`
    /// (a terminal failure).
    pub fn to_failed(self, reason: FailReason) -> Result<Self, InvalidTransition> {
        match self {
            Self::Preparing | Self::Starting | Self::Waiting(_) | Self::Paused | Self::Running => {
                Ok(Self::Failed(reason))
            }
            other => Err(other.invalid("failed", None)),
        }
    }
}

fn wait_reason_label(reason: &WaitReason) -> String {
    match reason {
        WaitReason::Participant(id) => format!("participant `{id}`"),
        WaitReason::ClockAbsent => "simulation clock".to_string(),
        WaitReason::ToolConnection(name) => format!("`{name}` connection"),
    }
}

fn fail_reason_label(reason: &FailReason) -> String {
    match reason {
        FailReason::Participant(id) => format!("participant `{id}` failed"),
        FailReason::Timeout => "timed out".to_string(),
        FailReason::Terminal(message) => message.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn running_clock() -> ClockObservation {
        ClockObservation { running: true }
    }

    fn paused_clock() -> ClockObservation {
        ClockObservation { running: false }
    }

    #[test]
    fn legal_edges_succeed() {
        assert_eq!(
            SessionState::Preparing.start().unwrap(),
            SessionState::Starting
        );

        assert_eq!(
            SessionState::Starting
                .to_waiting(WaitReason::ClockAbsent)
                .unwrap(),
            SessionState::Waiting(WaitReason::ClockAbsent)
        );
        assert_eq!(
            SessionState::Starting.to_paused(paused_clock()).unwrap(),
            SessionState::Paused
        );
        assert_eq!(
            SessionState::Starting.to_running().unwrap(),
            SessionState::Running
        );
        assert_eq!(
            SessionState::Starting
                .to_failed(FailReason::Timeout)
                .unwrap(),
            SessionState::Failed(FailReason::Timeout)
        );
        assert_eq!(
            SessionState::Starting.to_stopping().unwrap(),
            SessionState::Stopping
        );

        let waiting = SessionState::Waiting(WaitReason::ClockAbsent);
        assert_eq!(
            waiting.clone().to_paused(paused_clock()).unwrap(),
            SessionState::Paused
        );
        assert_eq!(waiting.clone().to_running().unwrap(), SessionState::Running);
        assert_eq!(
            waiting.clone().to_failed(FailReason::Timeout).unwrap(),
            SessionState::Failed(FailReason::Timeout)
        );
        assert_eq!(waiting.to_stopping().unwrap(), SessionState::Stopping);

        assert_eq!(
            SessionState::Paused.to_running().unwrap(),
            SessionState::Running
        );
        assert_eq!(
            SessionState::Paused
                .to_waiting(WaitReason::Participant("router".into()))
                .unwrap(),
            SessionState::Waiting(WaitReason::Participant("router".into()))
        );
        assert_eq!(
            SessionState::Paused.to_failed(FailReason::Timeout).unwrap(),
            SessionState::Failed(FailReason::Timeout)
        );
        assert_eq!(
            SessionState::Paused.to_stopping().unwrap(),
            SessionState::Stopping
        );

        assert_eq!(
            SessionState::Running
                .to_waiting(WaitReason::ClockAbsent)
                .unwrap(),
            SessionState::Waiting(WaitReason::ClockAbsent)
        );
        assert_eq!(
            SessionState::Running.to_paused(paused_clock()).unwrap(),
            SessionState::Paused
        );
        assert_eq!(
            SessionState::Running.to_stopping().unwrap(),
            SessionState::Stopping
        );
        assert_eq!(
            SessionState::Running
                .to_failed(FailReason::Timeout)
                .unwrap(),
            SessionState::Failed(FailReason::Timeout)
        );

        assert_eq!(
            SessionState::Stopping.to_stopped().unwrap(),
            SessionState::Stopped
        );

        // Preparing also accepts Ctrl-C and an early terminal failure.
        assert_eq!(
            SessionState::Preparing.to_stopping().unwrap(),
            SessionState::Stopping
        );
        assert_eq!(
            SessionState::Preparing
                .to_failed(FailReason::Terminal("boom".into()))
                .unwrap(),
            SessionState::Failed(FailReason::Terminal("boom".into()))
        );
    }

    #[test]
    fn illegal_edges_error_instead_of_panicking() {
        assert!(
            SessionState::Preparing
                .to_waiting(WaitReason::ClockAbsent)
                .is_err()
        );
        assert!(SessionState::Preparing.to_paused(paused_clock()).is_err());
        assert!(SessionState::Preparing.to_running().is_err());

        assert!(SessionState::Starting.start().is_err());
        assert!(
            SessionState::Waiting(WaitReason::ClockAbsent)
                .to_waiting(WaitReason::ClockAbsent)
                .is_err()
        );
        assert!(SessionState::Running.to_running().is_err());

        // Stopping resolves only to Stopped.
        let stopping = SessionState::Stopping;
        assert!(
            stopping
                .clone()
                .to_waiting(WaitReason::ClockAbsent)
                .is_err()
        );
        assert!(stopping.clone().to_paused(paused_clock()).is_err());
        assert!(stopping.clone().to_running().is_err());
        assert!(stopping.clone().to_failed(FailReason::Timeout).is_err());
        assert!(stopping.to_stopping().is_err());
    }

    #[test]
    fn terminal_states_reject_every_transition() {
        let stopped = SessionState::Stopped;
        assert!(stopped.clone().start().is_err());
        assert!(stopped.clone().to_waiting(WaitReason::ClockAbsent).is_err());
        assert!(stopped.clone().to_paused(paused_clock()).is_err());
        assert!(stopped.clone().to_running().is_err());
        assert!(stopped.clone().to_stopping().is_err());
        assert!(stopped.clone().to_stopped().is_err());
        assert!(stopped.to_failed(FailReason::Timeout).is_err());
        assert!(SessionState::Stopped.is_terminal());
        assert!(!SessionState::Stopped.is_active());

        let failed = SessionState::Failed(FailReason::Timeout);
        assert!(failed.clone().start().is_err());
        assert!(failed.clone().to_waiting(WaitReason::ClockAbsent).is_err());
        assert!(failed.clone().to_paused(paused_clock()).is_err());
        assert!(failed.clone().to_running().is_err());
        assert!(failed.clone().to_stopping().is_err());
        assert!(failed.clone().to_stopped().is_err());
        assert!(failed.clone().to_failed(FailReason::Timeout).is_err());
        assert!(failed.is_terminal());
    }

    #[test]
    fn paused_requires_an_observed_clock_sample() {
        // A running clock sample must never be accepted as evidence for Paused,
        // even from an otherwise-legal source state.
        let err = SessionState::Starting
            .to_paused(running_clock())
            .unwrap_err();
        assert_eq!(err.from, "starting");
        assert_eq!(err.attempted, "paused");
        assert!(err.detail.is_some());

        assert!(
            SessionState::Waiting(WaitReason::ClockAbsent)
                .to_paused(running_clock())
                .is_err()
        );
        assert!(SessionState::Running.to_paused(running_clock()).is_err());

        // The same sources succeed once the observation actually shows
        // running: false.
        assert!(SessionState::Starting.to_paused(paused_clock()).is_ok());
        assert!(
            SessionState::Waiting(WaitReason::ClockAbsent)
                .to_paused(paused_clock())
                .is_ok()
        );
        assert!(SessionState::Running.to_paused(paused_clock()).is_ok());
    }

    #[test]
    fn labels_are_human_readable() {
        assert_eq!(SessionState::Paused.label(), "paused");
        assert_eq!(SessionState::Running.label(), "running");
        assert_eq!(
            SessionState::Waiting(WaitReason::ClockAbsent).label(),
            "waiting for simulation clock"
        );
        assert_eq!(
            SessionState::Waiting(WaitReason::Participant("router".into())).label(),
            "waiting for participant `router`"
        );
        assert_eq!(
            SessionState::Failed(FailReason::Timeout).label(),
            "failed: timed out"
        );
    }

    #[test]
    fn invalid_transition_display_includes_states_and_detail() {
        let err = SessionState::Preparing.to_running().unwrap_err();
        let rendered = err.to_string();
        assert!(rendered.contains("preparing"));
        assert!(rendered.contains("running"));

        let err = SessionState::Starting
            .to_paused(running_clock())
            .unwrap_err();
        assert!(err.to_string().contains("running: false"));
    }
}
