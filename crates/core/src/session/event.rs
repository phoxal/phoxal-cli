//! The typed session lifecycle event: the single vocabulary
//! the root session controller uses to drive presentation from one bounded
//! channel.
//!
//! Nothing here depends on `supervisor`, `tui`, or `telemetry`, keeping the
//! dependency pointed one way so this module (and its tests) build and run
//! without pulling in the terminal/process/runtime machinery.
//!
//! Finding A4/C2: this vocabulary used to also carry `ParticipantChanged` and
//! `Telemetry` variants (plus their `ParticipantStatusLite`/
//! `ParticipantLifecycle`/`TelemetrySampleLite` payload types) as forward-
//! looking scaffolding for a "fully event-sourced" renderer. Neither was ever
//! constructed by production code: participant rows are read straight off
//! `supervisor::BoardSnapshot` (board polling, not events - see
//! the session controller's own docs on why that stays the source of truth)
//! and live telemetry flows through
//! `telemetry::TelemetryBackend`/`stores::telemetry_store::TelemetryStore`
//! instead. Removed rather than kept "for later" (YAGNI) - this event
//! vocabulary should only ever carry what a real producer emits.

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

    /// Test-only in practice (P4/C2 triage): production code compares/
    /// displays a `PhaseId` through its `PartialEq`/`Display` impls instead,
    /// but tests asserting on a real production-emitted id (e.g.
    /// `native_artifacts`'s download-phase tests) want a borrowed `&str`
    /// without allocating a `String` just to compare it - kept for that.
    #[must_use]
    #[allow(dead_code)]
    pub fn as_str(&self) -> &str {
        &self.0
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
///
/// `Skipped` is not for phases that never started - the plan forbids
/// pre-rendering future phases as skipped. It exists only for a phase that
/// genuinely began (emitted [`PhaseStarted`](SessionEvent::PhaseStarted))
/// and then found no work to do, e.g. a validation phase with nothing to
/// validate.
///
/// P4/C2 triage: none of the three phases wired so far (download/build/
/// validate - finding A3) ever produces `Skipped`, since each is gated to
/// only start when it has confirmed real work; the TUI renderer already
/// handles it correctly, tested directly. Kept rather than removed: a future phase that
/// starts and then finds nothing to do (e.g. a cache-hit build) is a real,
/// anticipated producer, not speculative scaffolding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PhaseOutcome {
    Succeeded,
    #[allow(dead_code)]
    Skipped,
    Failed {
        error: String,
    },
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
    /// Output attributed to one named standard tool (router, joypad,
    /// Webots, ...).
    ///
    /// P4/C2 triage: no current producer exists - `Ui::command_status_captured`
    /// attributes every captured child's output to the generic `Dependency`
    /// bucket regardless of which participant produced it. Per-participant
    /// attribution here would need that capture path to carry participant
    /// identity through, which is a real, separate feature (flagged as a
    /// follow-up), not something this refactor's scope covers. Kept rather
    /// than removed: this is documented, tested (`label()`) design intent
    /// for that follow-up, not speculative parallel scaffolding.
    #[allow(dead_code)]
    Tool { name: String },
    /// Output attributed to the supervisor itself. Same status as `Tool`
    /// above.
    #[allow(dead_code)]
    Supervisor,
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
        assert_eq!(id.as_str(), "router");
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
