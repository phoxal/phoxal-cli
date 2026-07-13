//! Routes the CLI's own `tracing` output through the session's
//! [`super::event::SessionEvent`] channel instead of writing it directly to
//! stderr, so a `tracing::warn!` mid-session (the reported "a Zenoh
//! connection warning writes through the progress renderer" bug) becomes a
//! typed [`super::event::SessionEvent::Diagnostic`] the renderer controls,
//! never a raw write racing an active redraw or corrupting the alternate
//! screen.
//!
//! Uses a process-global cell for the session's event sender (unlike
//! `crate::progress`'s spinner/bar, which take their `OutputMode` as an
//! explicit parameter): [`install`] is called once by
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
use std::sync::{Mutex, OnceLock};

use tokio::sync::mpsc;
use tracing_subscriber::fmt::MakeWriter;

use super::event::{DiagnosticLevel, DiagnosticSource, SessionEvent};

fn sender_cell() -> &'static Mutex<Option<mpsc::Sender<SessionEvent>>> {
    static CELL: OnceLock<Mutex<Option<mpsc::Sender<SessionEvent>>>> = OnceLock::new();
    CELL.get_or_init(|| Mutex::new(None))
}

/// Start routing the CLI's own tracing output through `events` instead of
/// stderr. Called once per session by `SessionController::new`.
pub fn install(events: mpsc::Sender<SessionEvent>) {
    *sender_cell()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(events);
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
        .clone()
}

/// Try to route one operator-facing message (`crate::ui::Ui::info`/`warn`/
/// `error`) through the active session's event channel instead of letting the
/// caller write it directly to stderr. Returns `true` only if the message was
/// actually enqueued (the caller must NOT also write to stderr - that would
/// print it twice, and the whole point is that stderr belongs to the renderer
/// during a session); `false` if no session is active OR the send itself
/// failed (channel full/closed - finding B1: a session that cannot accept the
/// message is not meaningfully "listening", so the caller must fall back to
/// its own direct write rather than silently losing the line).
pub(crate) fn try_route(source: DiagnosticSource, level: DiagnosticLevel, message: &str) -> bool {
    let Some(sender) = current_sender() else {
        return false;
    };
    sender
        .try_send(SessionEvent::Diagnostic {
            source,
            level,
            message: message.to_string(),
        })
        .is_ok()
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
            let message = String::from_utf8_lossy(buf).trim_end().to_string();
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
/// `#[tokio::test]` `async fn`s; `progress`, whose `spinner`/`bytes_bar` call
/// [`try_route`] and must see NO session installed to exercise their own
/// `Silent`/`Plain`/`Rich` fallback from plain, synchronous `#[test]` `fn`s).
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
}
