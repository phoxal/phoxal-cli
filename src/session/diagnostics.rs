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
/// caller write it directly to stderr. Returns `true` if a session was
/// listening (the caller must NOT also write to stderr - that would print it
/// twice, and the whole point is that stderr belongs to the renderer during a
/// session); `false` if no session is active, in which case the caller keeps
/// its own direct write exactly as before.
pub(crate) fn try_route(source: DiagnosticSource, level: DiagnosticLevel, message: &str) -> bool {
    let Some(sender) = current_sender() else {
        return false;
    };
    let _ = sender.try_send(SessionEvent::Diagnostic {
        source,
        level,
        message: message.to_string(),
    });
    true
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;

    // The sender cell is process-global; serialize the tests that touch it,
    // matching `progress`'s own `TEST_LOCK` precedent.
    static TEST_LOCK: StdMutex<()> = StdMutex::new(());

    #[test]
    fn install_routes_writes_as_diagnostic_events_instead_of_stderr() {
        let _guard = TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
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
        let _guard = TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
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
