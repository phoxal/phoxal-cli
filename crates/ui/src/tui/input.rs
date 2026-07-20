//! Bridge crossterm's blocking event reader onto the async world: a
//! dedicated OS thread polls/reads terminal events and forwards them over a
//! bounded channel, so `supervise_until_shutdown`'s `select!` loop can
//! `.recv()` them like any other async source (see `Display::next_input`).
//!
//! crossterm's `event::read()` blocks the calling thread; there is no
//! Tokio-native way to poll it without either this bridge thread or pulling
//! in crossterm's `event-stream` feature (and `futures`) for an
//! `EventStream`. The bridge thread is simpler and adds no new dependency.

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock, PoisonError, Weak};
use std::time::Duration;

use crossterm::event::{self, Event};
use tokio::sync::mpsc;

/// How long each `event::poll` call blocks before checking `stop` again -
/// bounds how quickly the reader thread notices [`InputThread::stop`] was
/// called.
const POLL_INTERVAL: Duration = Duration::from_millis(100);
const JOIN_TIMEOUT: Duration = Duration::from_millis(250);

/// Overflow policy: newest-wins. The redraw loop drains this every frame, so
/// in practice it never holds more than one or two events; this capacity is
/// only a backstop against a genuinely stalled consumer. If it ever fills,
/// the freshest keystroke/resize is worth more than a stale one, so a full
/// channel drops the incoming event rather than blocking the reader thread
/// (which would stall crossterm's own internal buffering) or growing without
/// bound.
const INPUT_CHANNEL_CAPACITY: usize = 256;

struct ReaderLifecycle {
    stop: AtomicBool,
    finished: Mutex<bool>,
    finished_changed: Condvar,
}

impl ReaderLifecycle {
    fn new() -> Self {
        Self {
            stop: AtomicBool::new(false),
            finished: Mutex::new(false),
            finished_changed: Condvar::new(),
        }
    }

    fn request_stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }

    fn should_stop(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }

    fn mark_finished(&self) {
        *self.finished.lock().unwrap_or_else(PoisonError::into_inner) = true;
        self.finished_changed.notify_all();
    }

    fn wait_finished_for(&self, wait: Duration) -> bool {
        let finished = self.finished.lock().unwrap_or_else(PoisonError::into_inner);
        let (finished, _) = self
            .finished_changed
            .wait_timeout_while(finished, wait, |finished| !*finished)
            .unwrap_or_else(PoisonError::into_inner);
        *finished
    }

    fn wait_finished(&self) {
        let mut finished = self.finished.lock().unwrap_or_else(PoisonError::into_inner);
        while !*finished {
            finished = self
                .finished_changed
                .wait(finished)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }
}

fn active_reader() -> &'static Mutex<Weak<ReaderLifecycle>> {
    static ACTIVE: OnceLock<Mutex<Weak<ReaderLifecycle>>> = OnceLock::new();
    ACTIVE.get_or_init(|| Mutex::new(Weak::new()))
}

fn register_active_reader(lifecycle: &Arc<ReaderLifecycle>) {
    *active_reader()
        .lock()
        .unwrap_or_else(PoisonError::into_inner) = Arc::downgrade(lifecycle);
}

fn clear_active_reader(lifecycle: &Arc<ReaderLifecycle>) {
    let mut active = active_reader()
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    if active
        .upgrade()
        .is_some_and(|registered| Arc::ptr_eq(&registered, lifecycle))
    {
        *active = Weak::new();
    }
}

/// Panic hooks run before ordinary field drops. Stop the active reader and
/// wait until it can no longer consume stdin before the hook restores cooked
/// terminal mode and delegates to the previous panic printer.
pub(super) fn stop_active_for_panic() {
    let lifecycle = active_reader()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .upgrade();
    if let Some(lifecycle) = lifecycle {
        lifecycle.request_stop();
        lifecycle.wait_finished();
    }
}

/// Handle to the background input-reading thread. `event::poll` is bounded by
/// [`POLL_INTERVAL`], so teardown can request stop and join before the TUI
/// restores the shell terminal, preventing the old reader from consuming a
/// keystroke meant for the prompt.
pub struct InputThread {
    lifecycle: Arc<ReaderLifecycle>,
    thread: Option<std::thread::JoinHandle<()>>,
    /// Set only when the thread ends because `event::poll`/`event::read`
    /// itself failed (the tty went away underneath the session) - never set
    /// by a clean [`Self::stop`] shutdown, so [`Self::take_error`] can tell
    /// the two apart. [`super::TuiDisplay::next_input`] surfaces this as a
    /// session failure instead of silently waiting on a channel that will
    /// never produce another event.
    error: Arc<Mutex<Option<io::Error>>>,
}

impl InputThread {
    /// Spawn the reader thread, returning both this handle and the receiving
    /// end of the channel it forwards events on.
    pub fn spawn() -> (Self, mpsc::Receiver<Event>) {
        let (tx, rx) = mpsc::channel(INPUT_CHANNEL_CAPACITY);
        let lifecycle = Arc::new(ReaderLifecycle::new());
        let error = Arc::new(Mutex::new(None));
        let thread_lifecycle = Arc::clone(&lifecycle);
        let thread_error = Arc::clone(&error);
        let thread = std::thread::spawn(move || {
            while !thread_lifecycle.should_stop() {
                match event::poll(POLL_INTERVAL) {
                    Ok(true) => match event::read() {
                        Ok(ev) => {
                            // Newest-wins (see `INPUT_CHANNEL_CAPACITY`): a
                            // full channel means the consumer is stalled, so
                            // drop this event rather than block the reader
                            // thread. `Closed` (the receiver is gone) still
                            // ends the thread, same as before.
                            match tx.try_send(ev) {
                                Ok(()) | Err(mpsc::error::TrySendError::Full(_)) => {}
                                Err(mpsc::error::TrySendError::Closed(_)) => break,
                            }
                        }
                        Err(read_error) => {
                            set_error(&thread_error, read_error);
                            break;
                        }
                    },
                    Ok(false) => {}
                    Err(poll_error) => {
                        set_error(&thread_error, poll_error);
                        break;
                    }
                }
            }
            thread_lifecycle.mark_finished();
        });
        register_active_reader(&lifecycle);
        (
            Self {
                lifecycle,
                thread: Some(thread),
                error,
            },
            rx,
        )
    }

    pub fn stop(&self) {
        self.lifecycle.request_stop();
    }

    pub fn stop_and_join(&mut self) {
        self.stop();
        if !self.try_join(JOIN_TIMEOUT) {
            tracing::warn!(
                "terminal input reader exceeded its join budget; retrying before terminal restoration"
            );
        }
    }

    fn try_join(&mut self, wait: Duration) -> bool {
        if self.thread.is_none() {
            return true;
        }
        if !self.lifecycle.wait_finished_for(wait) {
            return false;
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        true
    }

    /// Take the terminal read/poll error that ended the reader thread, if
    /// any - `None` means either the thread is still running, or it ended
    /// via a clean [`Self::stop`] call rather than a failure.
    pub fn take_error(&self) -> Option<io::Error> {
        self.error
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
    }
}

fn set_error(slot: &Mutex<Option<io::Error>>, error: io::Error) {
    *slot.lock().unwrap_or_else(PoisonError::into_inner) = Some(error);
}

impl Drop for InputThread {
    fn drop(&mut self) {
        self.stop();
        if !self.try_join(JOIN_TIMEOUT) {
            tracing::error!(
                "terminal input reader did not stop after a second join budget; waiting before terminal restoration"
            );
            // Terminal safety wins on this impossible-under-the-poll-contract
            // fallback: never restore cooked mode while a detached reader can
            // still consume the shell's next keystroke.
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
        clear_active_reader(&self.lifecycle);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_timed_out_join_keeps_the_handle_for_the_restoration_retry() {
        use std::sync::mpsc as std_mpsc;

        let lifecycle = Arc::new(ReaderLifecycle::new());
        let error = Arc::new(Mutex::new(None));
        let (release_tx, release_rx) = std_mpsc::channel();
        let thread_lifecycle = Arc::clone(&lifecycle);
        let thread = std::thread::spawn(move || {
            release_rx.recv().expect("release test reader");
            thread_lifecycle.mark_finished();
        });
        let mut input = InputThread {
            lifecycle,
            thread: Some(thread),
            error,
        };

        input.stop_and_join();
        assert!(
            input.thread.is_some(),
            "the first timeout must retain the join handle"
        );
        release_tx.send(()).expect("release test reader");
        input.stop_and_join();
        assert!(input.thread.is_none());
    }
}
