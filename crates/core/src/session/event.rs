//! The typed session lifecycle event: the single vocabulary
//! the root session controller uses to drive presentation from one bounded
//! channel.
//!
//! Nothing here depends on `supervisor`, `tui`, or `telemetry`, keeping the
//! dependency pointed one way so this module (and its tests) build and run
//! without pulling in the terminal/process/runtime machinery.
//!
//! This vocabulary carries only events with real producers. Participant rows
//! are read straight off
//! `supervisor::BoardSnapshot` (board polling, not events - see
//! the session controller's own docs on why that stays the source of truth)
//! and live telemetry flows through
//! `telemetry::TelemetryBackend`/`stores::telemetry_store::TelemetryStore`
//! instead.

use std::time::Duration;

/// Identifies one startup or runtime phase (`"download"`, `"build"`,
/// `"router"`, `"webots"`, ...).
///
/// Phases are dynamic: they are named by whatever operation actually starts,
/// never pre-declared from a fixed list (see the plan's "show only work that
/// has actually begun" decision), so this is an owned-string newtype rather
/// than a closed enum. `Eq + Hash` lets a renderer key rows by id; the
/// `Box<str>` keeps a stored id cheap to clone without carrying spare
/// `String` capacity.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PhaseId(Box<str>);

impl PhaseId {
    #[must_use]
    pub fn new(id: impl Into<Box<str>>) -> Self {
        Self(id.into())
    }
}

impl<T> From<T> for PhaseId
where
    T: Into<Box<str>>,
{
    fn from(id: T) -> Self {
        Self::new(id)
    }
}

impl std::fmt::Display for PhaseId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// How a started phase concluded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PhaseOutcome {
    Succeeded,
    Failed { error: String },
}

/// Where a [`SessionEvent::Diagnostic`] originated, so the renderer can
/// label and route it without string-matching a message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiagnosticSource {
    /// A `tracing` log record captured instead of written directly to
    /// stderr.
    Tracing,
    /// An operator-facing message from the CLI's own command code
    /// (the root CLI UI adapter) captured instead of written
    /// directly to stderr, so it cannot race a renderer's redraw.
    Cli,
    /// Output captured from a dependency's own logging (e.g. a library that
    /// writes to stderr directly).
    Dependency,
}

/// Severity of a [`SessionEvent::Diagnostic`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum DiagnosticLevel {
    Info,
    Warn,
    Error,
}

/// The one typed lifecycle event stream a `SessionController` uses to drive
/// the TUI.
///
/// The operation performing work emits its own phase events; nothing here
/// reconstructs progress by polling other state.
#[derive(Debug, Clone)]
pub enum SessionEvent {
    PhaseStarted {
        id: PhaseId,
        label: String,
    },
    PhaseProgress {
        id: PhaseId,
        completed: u64,
        total: u64,
        detail: Option<String>,
    },
    PhaseFinished {
        id: PhaseId,
        outcome: PhaseOutcome,
        elapsed: Duration,
    },
    Diagnostic {
        source: DiagnosticSource,
        level: DiagnosticLevel,
        message: String,
    },
    SessionChanged {
        state: super::state::SessionState,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn phase_id_equality_and_hashing() {
        let a = PhaseId::new("download");
        let b = PhaseId::new("download");
        let c = PhaseId::new("build");
        assert_eq!(a, b);
        assert_ne!(a, c);

        let mut set = HashSet::new();
        set.insert(a.clone());
        assert!(set.contains(&b));
        assert!(!set.contains(&c));
    }

    #[test]
    fn phase_id_from_str_and_display() {
        let id: PhaseId = "router".into();
        assert_eq!(id.to_string(), "router");
    }

    #[test]
    fn session_event_is_debug_and_clone() {
        let event = SessionEvent::PhaseStarted {
            id: PhaseId::new("download"),
            label: "Downloading artifacts".to_string(),
        };
        let cloned = event.clone();
        // `Debug` must be available for logging; exercise it directly.
        assert_eq!(format!("{event:?}"), format!("{cloned:?}"));
    }
}
