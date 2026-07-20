//! Routes the CLI's own `tracing` output through the session's
//! [`super::event::SessionEvent`] channel instead of writing it directly to
//! stderr, so a `tracing::warn!` mid-session (the reported "a Zenoh
//! connection warning writes through the progress renderer" bug) becomes a
//! typed [`super::event::SessionEvent::Diagnostic`] the renderer controls,
//! never a raw write racing an active redraw or corrupting the alternate
//! screen.
//!
//! Uses a process-global cell for the session's event sender. [`install`] is
//! called once by
//! [`super::controller::SessionController::new`] for the lifetime of one
//! `run`/`simulation run` session; [`uninstall`] restores direct stderr
//! writing on teardown. Every OTHER verb (and any code running before a
//! session starts) never calls [`install`], so [`SessionWriter`] simply
//! forwards to the real stderr - this module changes nothing for a caller
//! that never opts in.
//!
//! Level is approximated as [`super::event::DiagnosticLevel::Warn`] for every
//! captured line: `tracing_subscriber::fmt`'s [`MakeWriter`] contract hands
//! this only the already-formatted bytes for one event, not its
//! `Level` - recovering the real level would need a full custom `Layer`
//! (`on_event`) rather than a writer swap. This is an accepted
//! approximation, not a correctness gap: the CLI's default `EnvFilter` is
//! `"warn"` (see `main::init_tracing`), so in practice every line that
//! reaches this writer already IS `WARN` or `ERROR`.

use std::io::{self, Write};
use std::process::Child;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use tokio::sync::mpsc;
use tracing_subscriber::fmt::MakeWriter;

use phoxal_cli_core::session::event::{
    DiagnosticLevel, DiagnosticSource, PhaseId, PhaseOutcome, SessionEvent,
};

struct ActiveSession {
    sender: mpsc::Sender<SessionEvent>,
    children: Vec<Arc<Mutex<Option<Child>>>>,
}

fn sender_cell() -> &'static Mutex<Option<ActiveSession>> {
    static CELL: OnceLock<Mutex<Option<ActiveSession>>> = OnceLock::new();
    CELL.get_or_init(|| Mutex::new(None))
}

/// Start routing the CLI's own tracing output through `events` instead of
/// stderr. Called once per session by `SessionController::new`.
pub fn install(events: mpsc::Sender<SessionEvent>) {
    *sender_cell()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(ActiveSession {
        sender: events,
        children: Vec::new(),
    });
}

/// Stop routing to a session channel; every subsequent tracing line writes
/// directly to stderr again. Called by `SessionController` teardown so a
/// dropped session's tracing does not silently vanish into a closed channel.
pub fn uninstall() {
    *sender_cell()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
}

fn current_sender() -> Option<mpsc::Sender<SessionEvent>> {
    sender_cell()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .as_ref()
        .map(|session| session.sender.clone())
}

/// Result of attempting to route a line through the current session. A full
/// or closed session still owns the terminal, so callers must not fall back
/// to a raw stdout/stderr write in that case.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RouteResult {
    NoSession,
    Routed,
    Dropped,
}

/// Try to route one operator-facing message (`crate::ui::Ui::info`/`warn`/
/// `error`) through the active session's event channel instead of letting the
/// caller write it directly to stderr. Diagnostics may be dropped under
/// backpressure, but a live session's terminal ownership is never lost.
pub(crate) fn try_route(
    source: DiagnosticSource,
    level: DiagnosticLevel,
    message: &str,
) -> RouteResult {
    let Some(sender) = current_sender() else {
        return RouteResult::NoSession;
    };
    match sender.try_send(SessionEvent::Diagnostic {
        source,
        level,
        message: message.to_string(),
    }) {
        Ok(()) => RouteResult::Routed,
        Err(_) => RouteResult::Dropped,
    }
}

/// Register a captured child owned by the active session. The controller
/// stops these before waiting on cancelled preparation/setup work.
pub(crate) fn register_child(child: Arc<Mutex<Option<Child>>>) {
    if let Some(session) = sender_cell()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .as_mut()
    {
        session.children.push(child);
    }
}

/// Stop tracking a child once its owner has reaped it.
pub(crate) fn unregister_child(child: &Arc<Mutex<Option<Child>>>) {
    if let Some(session) = sender_cell()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .as_mut()
    {
        session
            .children
            .retain(|tracked| !Arc::ptr_eq(tracked, child));
    }
}

/// Best-effort immediate stop for every captured child owned by the active
/// session. A child may already have completed, so kill errors are harmless.
pub(crate) fn kill_active_children() {
    let children = sender_cell()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .as_ref()
        .map(|session| session.children.clone())
        .unwrap_or_default();
    for child in children {
        let mut child = child
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(child) = child.as_mut() else {
            continue;
        };
        #[cfg(unix)]
        if let Ok(group) = i32::try_from(child.id()) {
            // Captured build commands are process-group leaders (see
            // `Ui::command_status_captured`), so cancellation reaches rustc,
            // linkers, and any other descendants as well as cargo itself.
            let _ = unsafe { libc::kill(-group, libc::SIGKILL) };
        }
        #[cfg(not(unix))]
        let _ = child.kill();
    }
}

/// Emit `SessionEvent::PhaseStarted` for one real per-operation phase
/// (`"download"`/`"build"`/`"validate"`/...) through the active session, if
/// any (finding A3) - a no-op when no session is installed, exactly like
/// [`try_route`]. Prefer [`run_phase`] at the call site; this and
/// [`phase_finished`] exist for the rare caller that cannot express its work
/// as one `FnOnce`.
pub(crate) fn phase_started(id: impl Into<PhaseId>, label: impl Into<String>) {
    if let Some(sender) = current_sender() {
        let _ = sender.try_send(SessionEvent::PhaseStarted {
            id: id.into(),
            label: label.into(),
        });
    }
}

/// Emit `SessionEvent::PhaseFinished` for a phase previously started with
/// [`phase_started`]. See [`run_phase`] for the paired, harder-to-misuse
/// helper most callers should reach for instead.
pub(crate) fn phase_finished(id: impl Into<PhaseId>, outcome: PhaseOutcome, elapsed: Duration) {
    if let Some(sender) = current_sender() {
        let _ = sender.try_send(SessionEvent::PhaseFinished {
            id: id.into(),
            outcome,
            elapsed,
        });
    }
}

/// Emit `SessionEvent::PhaseProgress` for the CURRENTLY ACTIVE phase `id`
/// (finding C2: the startup surface's `PhaseRow::progress` only ever paired
/// with a test-constructed event before this - see
/// `native_artifacts::prepare_and_activate_descriptors`'s real
/// per-batch-completed/total download progress for the first genuine
/// producer). A no-op when no session is installed, exactly like
/// [`try_route`]; the renderer itself also ignores a progress update whose
/// `id` does not match its current phase (see `tui::startup::StartupState`).
pub(crate) fn phase_progress(id: impl Into<PhaseId>, completed: u64, total: u64) {
    if let Some(sender) = current_sender() {
        let _ = sender.try_send(SessionEvent::PhaseProgress {
            id: id.into(),
            completed,
            total,
            detail: None,
        });
    }
}

/// Bracket `work` with a `PhaseStarted`/`PhaseFinished` pair through the
/// active session's event channel, if any (finding A3) - the renderer then
/// shows real per-operation progress (download/build/validate) instead of
/// one opaque synthetic "Preparing" phase around the whole callback. A no-op
/// wrapper (falls straight through to `work`'s own result) when no session is
/// installed, exactly like [`try_route`].
///
/// Call this only once real work for `id` is confirmed to exist - e.g. a
/// non-empty download/build set. This helper does NOT itself decide whether
/// the operation should run at all (Product decision 3: a phase with no real
/// work behind it must never appear), so gate the call site on that decision
/// first.
///
/// Uses `try_send` (via [`phase_started`]/[`phase_finished`]), not an
/// awaited/blocking send: the controller's own select loop
/// (`drive_prepare_phase`/`drive_setup`) is ALWAYS draining `events_rx` for
/// the entire window a phase can run in - that is what makes the phase
/// visible at all - so this channel practically never fills. An awaited
/// `blocking_send` would also risk a genuine panic: this function runs from
/// synchronous code that is not always on a dedicated `spawn_blocking`
/// thread (`crate::simulation::live_simulate_setup` calls synchronous
/// build code directly from a plain `tokio::spawn`ed async task), and
/// `blocking_send` panics if called from a runtime's own worker thread.
pub(crate) fn run_phase<T>(
    id: impl Into<PhaseId>,
    label: impl Into<String>,
    work: impl FnOnce() -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    let id = id.into();
    phase_started(id.clone(), label);
    let started = Instant::now();
    let result = work();
    let outcome = match &result {
        Ok(_) => PhaseOutcome::Succeeded,
        Err(error) => PhaseOutcome::Failed {
            error: format!("{error:#}"),
        },
    };
    phase_finished(id, outcome, started.elapsed());
    result
}

/// The [`MakeWriter`] installed on the process-wide `tracing_subscriber::fmt`
/// layer in `main`. Stable for the process's whole lifetime - which session
/// (if any) is currently listening is decided per-write by [`current_sender`],
/// not by rebuilding the subscriber.
#[derive(Debug, Clone, Copy, Default)]
pub struct SessionAwareWriter;

impl<'a> MakeWriter<'a> for SessionAwareWriter {
    type Writer = SessionWriter;

    fn make_writer(&'a self) -> Self::Writer {
        SessionWriter
    }
}

#[derive(Debug)]
pub struct SessionWriter;

impl Write for SessionWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if let Some(sender) = current_sender() {
            // `main::init_tracing` enables ANSI whenever stderr is a real
            // terminal - true for an active TUI session too (raw mode/the
            // alternate screen do not change the underlying fd's
            // terminal-ness), so a captured line can carry real escape
            // codes here. The renderer applies its OWN theme color by
            // `DiagnosticLevel`/`DiagnosticSource` (`tui::render::
            // format_diagnostic`) and shows the message as plain text in a
            // `ratatui::text::Span`, which does not interpret escape
            // sequences - stripped here so a routed diagnostic reads as
            // text, not garbled control bytes, while the non-session
            // fallback below stays byte-for-byte unchanged (plain stderr
            // still gets real color).
            let message = console::strip_ansi_codes(&String::from_utf8_lossy(buf))
                .trim_end()
                .to_string();
            if !message.is_empty() {
                // Never block the tracing call site on a full channel - a
                // dropped diagnostic under backpressure is preferable to a
                // stalled log call. `try_send` also works from a plain sync
                // context, which `tracing_subscriber::fmt` requires (no
                // executor available at this call site).
                let _ = sender.try_send(SessionEvent::Diagnostic {
                    source: DiagnosticSource::Tracing,
                    level: DiagnosticLevel::Warn,
                    message,
                });
            }
            return Ok(buf.len());
        }
        io::stderr().write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        io::stderr().flush()
    }
}

/// The `sender_cell()` this module installs into is process-global, and it
/// is now read from OTHER modules' tests too (`session::controller`, which
/// constructs a real `SessionController` and so calls [`install`] from
/// `#[tokio::test]` `async fn`s; `progress`, whose `status` calls
/// [`try_route`] and must see NO session installed to exercise their own
/// `Silent`/`Plain`/`Routed` fallback from plain, synchronous `#[test]` `fn`s).
/// Every test anywhere in the crate that installs, uninstalls, or depends on
/// nothing being installed MUST acquire this lock first, or two such tests
/// running concurrently (the default `cargo test` behavior) can flip each
/// other's expected state and fail intermittently.
///
/// A `tokio::sync::Mutex`, not `std::sync::Mutex`: a guard needs to stay held
/// across an `.await` in `session::controller`'s async tests (the whole point
/// is serializing for the FULL test, including an awaited `drive_setup`/
/// `drive_prepare_phase` call, not just the synchronous `install()` moment) -
/// `clippy::await_holding_lock` correctly forbids that for a `std::sync`
/// guard. A synchronous `#[test]` fn (this module's own tests, `progress`'s)
/// uses [`tokio::sync::Mutex::blocking_lock`] instead, which is exactly the
/// sync-context escape hatch this type provides.
#[cfg(test)]
pub(crate) static DIAGNOSTICS_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_routes_writes_as_diagnostic_events_instead_of_stderr() {
        let _guard = DIAGNOSTICS_TEST_LOCK.blocking_lock();
        let (tx, mut rx) = mpsc::channel(8);
        install(tx);

        let mut writer = SessionWriter;
        writer
            .write_all(b"zenoh: connection warning\n")
            .expect("write must succeed");

        let event = rx.try_recv().expect("a diagnostic event must be queued");
        match event {
            SessionEvent::Diagnostic {
                source,
                level,
                message,
            } => {
                assert_eq!(source, DiagnosticSource::Tracing);
                assert_eq!(level, DiagnosticLevel::Warn);
                assert_eq!(message, "zenoh: connection warning");
            }
            other => panic!("expected a Diagnostic event, got {other:?}"),
        }

        uninstall();
    }

    #[test]
    fn full_active_session_drops_instead_of_claiming_no_session() {
        let _guard = DIAGNOSTICS_TEST_LOCK.blocking_lock();
        let (tx, _rx) = mpsc::channel(1);
        install(tx.clone());
        tx.try_send(SessionEvent::Diagnostic {
            source: DiagnosticSource::Cli,
            level: DiagnosticLevel::Info,
            message: "fill the channel".to_string(),
        })
        .expect("the first event fills the channel");

        assert_eq!(
            try_route(
                DiagnosticSource::Cli,
                DiagnosticLevel::Warn,
                "must not print raw"
            ),
            RouteResult::Dropped
        );
        uninstall();
    }

    #[test]
    fn uninstall_falls_back_to_direct_stderr_writes() {
        let _guard = DIAGNOSTICS_TEST_LOCK.blocking_lock();
        uninstall();
        let mut writer = SessionWriter;
        // Must not panic and must report every byte written, exactly like a
        // real stderr write would.
        let written = writer
            .write(b"hello\n")
            .expect("fallback write must succeed");
        assert_eq!(written, 6);
    }

    /// Finding A3: `run_phase` must emit a real `PhaseStarted`/`PhaseFinished`
    /// pair around genuine work, not the old synthetic single "Preparing"
    /// phase - and must return `work`'s own value untouched.
    #[test]
    fn run_phase_emits_a_started_and_finished_pair_and_returns_work_s_value() {
        let _guard = DIAGNOSTICS_TEST_LOCK.blocking_lock();
        let (tx, mut rx) = mpsc::channel(8);
        install(tx);

        let result = run_phase(PhaseId::new("download"), "Downloading artifacts", || {
            Ok::<_, anyhow::Error>(42)
        });
        assert_eq!(result.unwrap(), 42);

        match rx.try_recv().expect("PhaseStarted must be queued") {
            SessionEvent::PhaseStarted { id, label } => {
                assert_eq!(id, PhaseId::new("download"));
                assert_eq!(label, "Downloading artifacts");
            }
            other => panic!("expected PhaseStarted, got {other:?}"),
        }
        match rx.try_recv().expect("PhaseFinished must be queued") {
            SessionEvent::PhaseFinished { id, outcome, .. } => {
                assert_eq!(id, PhaseId::new("download"));
                assert_eq!(outcome, PhaseOutcome::Succeeded);
            }
            other => panic!("expected PhaseFinished, got {other:?}"),
        }

        uninstall();
    }

    /// A phase whose work fails must still emit `PhaseFinished` (with a
    /// `Failed` outcome carrying the error), not leave the renderer showing a
    /// phase that started but never concluded.
    #[test]
    fn run_phase_reports_a_failed_outcome_and_still_propagates_the_error() {
        let _guard = DIAGNOSTICS_TEST_LOCK.blocking_lock();
        let (tx, mut rx) = mpsc::channel(8);
        install(tx);

        let result = run_phase(PhaseId::new("build"), "Building thing", || {
            Err::<(), _>(anyhow::anyhow!("cargo build failed"))
        });
        assert_eq!(result.unwrap_err().to_string(), "cargo build failed");

        let _started = rx.try_recv().expect("PhaseStarted must be queued");
        match rx.try_recv().expect("PhaseFinished must be queued") {
            SessionEvent::PhaseFinished { outcome, .. } => match outcome {
                PhaseOutcome::Failed { error } => assert_eq!(error, "cargo build failed"),
                other => panic!("expected a Failed outcome, got {other:?}"),
            },
            other => panic!("expected PhaseFinished, got {other:?}"),
        }

        uninstall();
    }

    /// No session installed: `run_phase` must fall straight through to
    /// `work`'s own result without panicking (mirrors `try_route`'s own
    /// no-session fallback).
    #[test]
    fn run_phase_is_a_transparent_passthrough_with_no_session_installed() {
        let _guard = DIAGNOSTICS_TEST_LOCK.blocking_lock();
        uninstall();
        let result = run_phase(PhaseId::new("validate"), "Validating", || {
            Ok::<_, anyhow::Error>("ok")
        });
        assert_eq!(result.unwrap(), "ok");
    }
}
