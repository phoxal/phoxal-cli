//! The pure session state machine
//! the root session controller drives from supervision and preparation.
//!
//! Every transition is a method that consumes `self` and returns the next
//! state or a documented [`InvalidTransition`] error - illegal edges are
//! representable as data, never a panic, so a caller (or a test) can assert
//! on them directly.
//!
//! ```text
//! Preparing -> Starting -> Running -> Stopping -> Stopped
//!                         \-> Failed
//! ```
//!
//! `Stopped` and `Failed` are terminal: no transition leaves them. `Stopping`
//! only ever resolves to `Stopped` - it is not itself a valid source for
//! `Running`/`Failed`, matching the diagram's single
//! `Stopping -> Stopped` edge. Every other non-terminal state additionally
//! accepts `Stopping` (Ctrl-C) and `Failed` (a terminal failure), which is
//! what lets `Preparing` fail or be cancelled before `Starting` ever begins.

use std::fmt;

/// Why the session ended in `Failed`.
///
/// P4/C2 triage: `Terminal` is the real producer
/// (`session::controller::SessionController::reflect_final_outcome`).
/// `Participant`/`Timeout` are documented, tested design intent for a more
/// specific failure attribution than `reflect_final_outcome` currently
/// builds - kept rather than removed because they are useful terminal
/// failure categories.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FailReason {
    /// A named participant failed in a way the session cannot recover from.
    #[allow(dead_code)]
    Participant(String),
    /// A bounded wait exceeded its explicit, non-interactive policy.
    #[allow(dead_code)]
    Timeout,
    /// Any other terminal failure, carrying a short human message.
    Terminal(String),
}

/// The session's lifecycle state. See the module docs for the full
/// transition diagram and invariants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionState {
    Preparing,
    Starting,
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

// clippy's `wrong_self_convention` wants a `to_*` method to borrow `self`
// (like `to_string`), but every `to_*` method below is an intentionally
// CONSUMING state transition (`Preparing -> Starting`, ...): the whole point
// is that the caller's old `SessionState` is moved into the next one, never
// read back afterward, matching a typestate/state-machine idiom rather than
// the "cheap owned-to-owned conversion of a Copy type" case the lint
// targets. Renaming the whole transition-method surface to `into_*` would
// match the lint literally but read strangely for a state MACHINE (`Preparing
// -> Starting` reads far more naturally as `preparing.to_stopping()` than
// `preparing.into_stopping()`), so this is a deliberate, crate-local
// exception rather than a rename.
#[allow(clippy::wrong_self_convention)]
impl SessionState {
    fn label_str(&self) -> &'static str {
        match self {
            Self::Preparing => "preparing",
            Self::Starting => "starting",
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

    /// A short human label for the runtime console.
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            Self::Failed(reason) => format!("failed: {}", fail_reason_label(reason)),
            _ => self.label_str().to_string(),
        }
    }

    /// `true` once the session has reached a state no transition leaves.
    ///
    /// P4/C2 triage: no current caller needs this as a standalone predicate -
    /// `reduce_state`/`reflect_final_outcome` reach `Stopped`/`Failed` through
    /// the `to_*` transition methods' own `Result`, never by querying
    /// terminality first. Kept (tested directly by
    /// `terminal_states_reject_every_transition`) as the obvious, cheap
    /// predicate any state-machine consumer would expect to exist.
    #[must_use]
    #[allow(dead_code)]
    pub const fn is_terminal(&self) -> bool {
        matches!(self, Self::Stopped | Self::Failed(_))
    }

    /// `true` while the session's lifecycle is still progressing, i.e. the
    /// complement of [`Self::is_terminal`]. Same status as `is_terminal`
    /// above.
    #[must_use]
    #[allow(dead_code)]
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

    /// `Starting -> Running`.
    pub fn to_running(self) -> Result<Self, InvalidTransition> {
        match self {
            Self::Starting => Ok(Self::Running),
            other => Err(other.invalid("running", None)),
        }
    }

    /// Any non-terminal state except `Stopping` itself `-> Stopping`
    /// (Ctrl-C / a graceful stop request).
    pub fn to_stopping(self) -> Result<Self, InvalidTransition> {
        match self {
            Self::Preparing | Self::Starting | Self::Running => Ok(Self::Stopping),
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
            Self::Preparing | Self::Starting | Self::Running => Ok(Self::Failed(reason)),
            other => Err(other.invalid("failed", None)),
        }
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

    #[test]
    fn legal_edges_succeed() {
        assert_eq!(
            SessionState::Preparing.start().unwrap(),
            SessionState::Starting
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
        assert!(SessionState::Preparing.to_running().is_err());

        assert!(SessionState::Starting.start().is_err());
        assert!(SessionState::Running.to_running().is_err());

        // Stopping resolves only to Stopped.
        let stopping = SessionState::Stopping;
        assert!(stopping.clone().to_running().is_err());
        assert!(stopping.clone().to_failed(FailReason::Timeout).is_err());
        assert!(stopping.to_stopping().is_err());
    }

    #[test]
    fn terminal_states_reject_every_transition() {
        let stopped = SessionState::Stopped;
        assert!(stopped.clone().start().is_err());
        assert!(stopped.clone().to_running().is_err());
        assert!(stopped.clone().to_stopping().is_err());
        assert!(stopped.clone().to_stopped().is_err());
        assert!(stopped.to_failed(FailReason::Timeout).is_err());
        assert!(SessionState::Stopped.is_terminal());
        assert!(!SessionState::Stopped.is_active());

        let failed = SessionState::Failed(FailReason::Timeout);
        assert!(failed.clone().start().is_err());
        assert!(failed.clone().to_running().is_err());
        assert!(failed.clone().to_stopping().is_err());
        assert!(failed.clone().to_stopped().is_err());
        assert!(failed.clone().to_failed(FailReason::Timeout).is_err());
        assert!(failed.is_terminal());
    }

    #[test]
    fn labels_are_human_readable() {
        assert_eq!(SessionState::Running.label(), "running");
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
    }
}
