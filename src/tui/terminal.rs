//! Terminal ownership: entering/leaving raw mode + the alternate screen, and
//! restoring the terminal on every exit path - normal quit, Ctrl-C (as a key
//! event, since raw mode disables the terminal's own SIGINT generation - see
//! [`crate::tui::input`]), and panic.
//!
//! The panic case is the one that needs care: by the time a `Drop` impl would
//! run during unwinding, Rust's default panic handler has *already* printed
//! the message/backtrace - while the terminal is still in raw/alternate-screen
//! state, which garbles or hides it. [`install_panic_hook`] chains a restore
//! in front of the previous hook so the terminal is sane again before
//! anything is printed, then still calls the previous hook so behavior
//! (backtraces, `RUST_BACKTRACE`, any other installed hook) is unchanged.
//!
//! [`TerminalGuard`]'s own `Drop` covers every *normal* return path (a `?`
//! early return unwinds the same local variables a plain `return` would, so
//! no call site needs its own cleanup) - the panic hook exists only to fix
//! the ordering problem above, not to duplicate the restore logic: both
//! funnel through [`restore_terminal_best_effort`], and a global flag makes
//! the second call (whichever of the two runs last) a no-op.

use std::io::{self, IsTerminal, Stderr};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Mutex, Once, OnceLock, PoisonError};

use crossterm::cursor::Show;
use crossterm::execute;
use crossterm::style::Print;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, SetTitle, disable_raw_mode, enable_raw_mode,
};

/// Set while a [`TerminalGuard`] is live, so the panic hook and `Drop` both
/// know whether there is anything to restore, and so the restore only ever
/// runs once (whichever of panic-hook-triggered or `Drop`-triggered fires
/// first wins; the other sees `false` and does nothing).
static TUI_ACTIVE: AtomicBool = AtomicBool::new(false);
static PANIC_HOOK_INSTALLED: Once = Once::new();
static PANIC_HOOK_INSTALL_COUNT: AtomicUsize = AtomicUsize::new(0);
const SAVE_WINDOW_TITLE: &str = "\x1b[22;0t";
const RESTORE_WINDOW_TITLE: &str = "\x1b[23;0t";

fn tui_owner() -> &'static Mutex<Option<std::thread::ThreadId>> {
    static OWNER: OnceLock<Mutex<Option<std::thread::ThreadId>>> = OnceLock::new();
    OWNER.get_or_init(|| Mutex::new(None))
}

fn current_thread_owns_tui() -> bool {
    tui_owner()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .as_ref()
        .is_some_and(|owner| *owner == std::thread::current().id())
}

/// The idempotency gate, split out from the actual terminal I/O below so it
/// can be unit-tested without ever writing a real escape sequence to this
/// process's own stderr (which - unlike a piped CI stderr - may be a real
/// terminal when a developer runs `cargo test` interactively, and a stray
/// `LeaveAlternateScreen` would visibly disrupt it even though nothing was
/// really entered). Returns `true` at most once per `true` stored by
/// [`TerminalGuard::enter`]: the first of a panic-hook-triggered and a
/// `Drop`-triggered restore to call this wins and performs the real I/O: the
/// other sees `false` and does nothing.
fn take_active() -> bool {
    TUI_ACTIVE
        .compare_exchange(true, false, Ordering::SeqCst, Ordering::SeqCst)
        .is_ok()
}

/// Best-effort terminal restore: make the cursor visible, leave the alternate
/// screen, and disable raw mode. Never panics - every step's error is swallowed, since this runs
/// from contexts (a panic hook, a `Drop` impl) that must not themselves
/// panic or fail. Idempotent via [`take_active`]: a second call after the
/// first has already restored is a no-op that performs no I/O at all.
fn restore_terminal_best_effort() {
    if !take_active() {
        return;
    }
    *tui_owner().lock().unwrap_or_else(PoisonError::into_inner) = None;
    let _ = disable_raw_mode();
    let _ = write_terminal_restore(&mut io::stderr());
}

fn write_terminal_restore(writer: &mut impl io::Write) -> io::Result<()> {
    execute!(
        writer,
        Show,
        LeaveAlternateScreen,
        Print(RESTORE_WINDOW_TITLE)
    )
}

/// Install the panic hook exactly once per process. Chains: capture whatever
/// hook is already installed, restore the terminal first (if one is active),
/// then call the captured hook so the panic message/backtrace still prints -
/// now onto a sane terminal.
pub(crate) fn install_panic_hook() {
    PANIC_HOOK_INSTALLED.call_once(|| {
        PANIC_HOOK_INSTALL_COUNT.fetch_add(1, Ordering::Relaxed);
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            if current_thread_owns_tui() {
                super::input::stop_active_for_panic();
                restore_terminal_best_effort();
                previous(info);
                return;
            }
            if TUI_ACTIVE.load(Ordering::SeqCst) {
                let _ = crate::session::diagnostics::try_route(
                    crate::session::event::DiagnosticSource::Cli,
                    crate::session::event::DiagnosticLevel::Error,
                    &format!("background thread panicked: {info}"),
                );
                // The owner keeps the alternate screen active. Printing the
                // previous hook to raw stderr here would corrupt a frame and
                // be erased by the next redraw.
                return;
            }
            let _ = crate::session::diagnostics::try_route(
                crate::session::event::DiagnosticSource::Cli,
                crate::session::event::DiagnosticLevel::Error,
                &format!("background thread panicked: {info}"),
            );
            previous(info);
        }));
    });
}

/// RAII terminal ownership: entering raw mode + the alternate screen on
/// construction, restoring both on drop. Construct only when
/// [`TerminalGuard::should_use_terminal`] is true - never on a piped stream.
pub struct TerminalGuard;

impl TerminalGuard {
    /// Whether a full-screen TUI may take over this process's stderr. The
    /// direct `is_terminal` check ensures a TUI never owns a pipe.
    #[must_use]
    pub fn should_use_terminal(stderr: &Stderr) -> bool {
        stderr.is_terminal()
    }

    /// Enter raw mode + the alternate screen and install the panic-hook
    /// chain. Returns `Err` (leaving the terminal untouched) if either
    /// terminal call fails, e.g. a `TERM` that does not support the
    /// alternate screen.
    pub fn enter(title: &str) -> io::Result<Self> {
        install_panic_hook();
        enable_raw_mode()?;
        if let Err(error) = execute!(io::stderr(), EnterAlternateScreen) {
            let _ = disable_raw_mode();
            return Err(error);
        }
        let _ = execute!(io::stderr(), Print(SAVE_WINDOW_TITLE), SetTitle(title));
        *tui_owner().lock().unwrap_or_else(PoisonError::into_inner) =
            Some(std::thread::current().id());
        TUI_ACTIVE.store(true, Ordering::SeqCst);
        Ok(Self)
    }

    /// Reassert the title after terminal initialization. Some embedded
    /// terminals apply their command-start title after alternate-screen entry,
    /// so the title set by [`Self::enter`] can otherwise be immediately lost.
    pub fn set_title(title: &str) -> io::Result<()> {
        execute!(io::stderr(), SetTitle(title))
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore_terminal_best_effort();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex as StdMutex;

    use super::*;

    // `TUI_ACTIVE` is process-global; serialize the tests that touch it so
    // they cannot interleave with each other under the test harness's
    // default parallelism (same pattern as `progress`'s `TEST_LOCK`).
    static TEST_LOCK: StdMutex<()> = StdMutex::new(());

    /// The guard-level idempotency gate (deliberately NOT exercising
    /// `restore_terminal_best_effort` itself, which performs real terminal
    /// I/O - see `take_active`'s docs on why that must stay untested here):
    /// taking the flag when nothing is active must yield `false`, safely and
    /// repeatably.
    #[test]
    fn take_active_is_a_safe_no_op_when_nothing_is_active() {
        let _guard = TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        TUI_ACTIVE.store(false, Ordering::SeqCst);
        assert!(!take_active());
        assert!(!take_active());
    }

    /// Simulates the "already active" state a real `enter()` would have set,
    /// and proves taking it both flips the flag and is idempotent against a
    /// second call (the panic-hook-then-Drop double-fire case) - the exact
    /// mechanism `restore_terminal_best_effort` relies on to perform its real
    /// I/O at most once.
    #[test]
    fn take_active_is_idempotent_across_two_calls() {
        let _guard = TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        TUI_ACTIVE.store(true, Ordering::SeqCst);
        assert!(take_active(), "the first take must observe the active flag");
        assert!(
            !take_active(),
            "a second take after the first must see nothing left to restore"
        );
    }

    /// Installing the panic hook twice must not panic or double-chain -
    /// `Once` guarantees the closure body runs exactly once per process.
    #[test]
    fn installing_the_panic_hook_twice_is_safe() {
        install_panic_hook();
        let installed = PANIC_HOOK_INSTALL_COUNT.load(Ordering::Relaxed);
        install_panic_hook();
        assert_eq!(installed, 1);
        assert_eq!(PANIC_HOOK_INSTALL_COUNT.load(Ordering::Relaxed), installed);
    }

    #[test]
    fn terminal_restore_leaves_the_title_stack_to_the_terminal() {
        let mut output = Vec::new();
        write_terminal_restore(&mut output).expect("render terminal restore");
        let output = String::from_utf8(output).expect("terminal commands are utf-8");
        let show = output.find("\x1b[?25h").expect("show cursor");
        let leave = output.find("\x1b[?1049l").expect("leave alternate screen");
        let title = output.find(RESTORE_WINDOW_TITLE).expect("restore title");
        assert!(show < leave && leave < title);
        assert!(!output.contains("\x1b]0;"), "must not blank the title");
    }

    #[test]
    fn only_the_entering_thread_owns_panic_restore() {
        let _guard = TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *tui_owner().lock().unwrap_or_else(PoisonError::into_inner) =
            Some(std::thread::current().id());
        assert!(current_thread_owns_tui());
        assert!(!std::thread::spawn(current_thread_owns_tui).join().unwrap());
        *tui_owner().lock().unwrap_or_else(PoisonError::into_inner) = None;
    }
}
