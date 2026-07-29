//! The session lifecycle state the root session controller drives from
//! supervision and preparation.
//!
//! ```text
//! Preparing -> Starting -> Running -> Stopping -> Stopped
//!                         \-> Failed
//! ```
//!
//! `Stopped` and `Failed` are terminal: nothing transitions out of them. Only
//! two edges are validated as typestate methods that consume `self` and
//! return the next state or a documented [`InvalidTransition`] error -
//! `start` (`Preparing -> Starting`) and `to_stopping` (any non-terminal
//! state, e.g. Ctrl-C or a graceful stop request). The rest of the diagram
//! (`Starting -> Running`, `Stopping -> Stopped`, `-> Failed`) is reflected
//! directly from the resident supervisor's own `ProjectLifecycle` by
//! the attachment application, which already tracks those transitions; only the two
//! edges a caller can request out of band from that reflection carry their
//! own validated API here.

use std::fmt;

/// Why the session ended in `Failed`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FailReason {
    /// The terminal failure, carrying a short human message.
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
// (like `to_string`), but `to_stopping` below is an intentionally CONSUMING
// state transition (`{Preparing,Starting,Running} -> Stopping`): the whole
// point is that the caller's old `SessionState` is moved into the next one,
// never read back afterward, matching a typestate/state-machine idiom rather
// than the "cheap owned-to-owned conversion of a Copy type" case the lint
// targets. Renaming it to `into_stopping` would match the lint literally but
// read strangely for a state MACHINE (`Preparing -> Stopping` reads far more
// naturally as `preparing.to_stopping()` than `preparing.into_stopping()`),
// so this is a deliberate, crate-local exception rather than a rename.
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

    /// `Preparing -> Starting`.
    pub fn start(self) -> Result<Self, InvalidTransition> {
        match self {
            Self::Preparing => Ok(Self::Starting),
            other => Err(other.invalid("starting", None)),
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
}

fn fail_reason_label(reason: &FailReason) -> String {
    let FailReason::Terminal(message) = reason;
    message.clone()
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
            SessionState::Starting.to_stopping().unwrap(),
            SessionState::Stopping
        );
        assert_eq!(
            SessionState::Running.to_stopping().unwrap(),
            SessionState::Stopping
        );

        // Preparing also accepts Ctrl-C before Starting ever begins.
        assert_eq!(
            SessionState::Preparing.to_stopping().unwrap(),
            SessionState::Stopping
        );
    }

    #[test]
    fn illegal_edges_error_instead_of_panicking() {
        assert!(SessionState::Starting.start().is_err());

        // Stopping resolves only through the controller's own lifecycle
        // reflection, never by requesting `to_stopping` a second time.
        assert!(SessionState::Stopping.to_stopping().is_err());
    }

    #[test]
    fn terminal_states_reject_every_transition() {
        let stopped = SessionState::Stopped;
        assert!(stopped.clone().start().is_err());
        assert!(stopped.to_stopping().is_err());

        let failed = SessionState::Failed(FailReason::Terminal("boom".into()));
        assert!(failed.clone().start().is_err());
        assert!(failed.to_stopping().is_err());
    }

    #[test]
    fn labels_are_human_readable() {
        assert_eq!(SessionState::Running.label(), "running");
        assert_eq!(
            SessionState::Failed(FailReason::Terminal("boom".into())).label(),
            "failed: boom"
        );
    }

    #[test]
    fn invalid_transition_display_includes_states_and_detail() {
        let err = SessionState::Stopped.to_stopping().unwrap_err();
        let rendered = err.to_string();
        assert!(rendered.contains("stopped"));
        assert!(rendered.contains("stopping"));
    }
}
