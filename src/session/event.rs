//! The typed session lifecycle event: the single vocabulary a
//! `SessionController` (added in a later slice) will use to drive both the
//! TUI and the plain/line renderer from one bounded channel.
//!
//! Nothing here depends on `supervisor`, `tui`, or `telemetry` - a later
//! slice maps the heavier session types into these. Keeping the dependency
//! pointed one way lets this module (and its tests) build and run without
//! pulling in the terminal/process/runtime machinery.

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

    #[must_use]
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PhaseOutcome {
    Succeeded,
    Skipped,
    Failed { error: String },
}

/// Where a [`SessionEvent::Diagnostic`] originated, so the renderer can
/// label and route it without string-matching a message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiagnosticSource {
    /// A `tracing` log record captured instead of written directly to
    /// stderr.
    Tracing,
    /// Output captured from a dependency's own logging (e.g. a library that
    /// writes to stderr directly).
    Dependency,
    /// Output attributed to one named standard tool (router, joypad,
    /// Webots, ...).
    Tool { name: String },
    /// Output attributed to the supervisor itself.
    Supervisor,
}

/// Severity of a [`SessionEvent::Diagnostic`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum DiagnosticLevel {
    Info,
    Warn,
    Error,
}

/// The lifecycle stage of one participant, independent of
/// `supervisor::ParticipantStatus`.
///
/// This is deliberately a small, separate type: this module must not depend
/// on the heavier supervisor session code, so a later slice maps supervisor
/// state into this rather than this module importing supervisor's richer
/// type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParticipantLifecycle {
    Starting,
    Ready,
    Running,
    Degraded,
    Failed,
    Stopped,
}

/// A minimal snapshot of one participant's lifecycle, carried by
/// [`SessionEvent::ParticipantChanged`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParticipantStatusLite {
    pub lifecycle: ParticipantLifecycle,
    /// A short human detail (last error, wait reason, ...), if any.
    pub detail: Option<String>,
}

impl ParticipantStatusLite {
    #[must_use]
    pub fn new(lifecycle: ParticipantLifecycle) -> Self {
        Self {
            lifecycle,
            detail: None,
        }
    }

    #[must_use]
    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }
}

/// A lightweight telemetry sample carried by [`SessionEvent::Telemetry`].
///
/// NOTE: `TelemetryStore` is being built in a parallel slice; this is a
/// deliberately minimal placeholder carrying only what a renderer needs to
/// show a live value (a source, a monotonic receive marker for freshness/
/// staleness and dedup-by-time, and named numeric metrics). A later slice
/// adapts the real telemetry sample type into this rather than this module
/// depending on the store.
#[derive(Debug, Clone, PartialEq)]
pub struct TelemetrySampleLite {
    /// Which participant/topic produced this sample.
    pub source: String,
    /// Monotonic receive time, used to dedupe by sample identity/time (not
    /// by equal values) and to detect staleness.
    pub received_at: std::time::Instant,
    /// Named numeric readings (e.g. `("cpu_percent", 4.2)`).
    pub metrics: Vec<(String, f64)>,
}

/// The one typed lifecycle event stream a `SessionController` will use to
/// drive both the TUI and the plain/line renderer.
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
    ParticipantChanged {
        id: String,
        status: ParticipantStatusLite,
    },
    Diagnostic {
        source: DiagnosticSource,
        level: DiagnosticLevel,
        message: String,
    },
    Telemetry {
        sample: TelemetrySampleLite,
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
    fn participant_status_lite_builder() {
        let status =
            ParticipantStatusLite::new(ParticipantLifecycle::Degraded).with_detail("clock absent");
        assert_eq!(status.lifecycle, ParticipantLifecycle::Degraded);
        assert_eq!(status.detail.as_deref(), Some("clock absent"));
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
