//! Bridge crossterm's blocking event reader onto the async world: a
//! dedicated OS thread polls/reads terminal events and forwards them over a
//! bounded channel, so `supervise_until_shutdown`'s `select!` loop can
//! `.recv()` them like any other async source (see `Display::next_input`).
//!
//! crossterm's `event::read()` blocks the calling thread; there is no
//! Tokio-native way to poll it without either this bridge thread or pulling
//! in crossterm's `event-stream` feature (and `futures`) for an
//! `EventStream`. The bridge thread is simpler and adds no new dependency.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crossterm::event::{self, Event};
use tokio::sync::mpsc;

/// How long each `event::poll` call blocks before checking `stop` again -
/// bounds how quickly the reader thread notices [`InputThread::stop`] was
/// called.
const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Overflow policy: newest-wins. The redraw loop drains this every frame, so
/// in practice it never holds more than one or two events; this capacity is
/// only a backstop against a genuinely stalled consumer. If it ever fills,
/// the freshest keystroke/resize is worth more than a stale one, so a full
/// channel drops the incoming event rather than blocking the reader thread
/// (which would stall crossterm's own internal buffering) or growing without
/// bound.
const INPUT_CHANNEL_CAPACITY: usize = 256;

/// Handle to the background input-reading thread. Not joined on drop (a
/// blocking `event::poll` cannot be interrupted mid-wait without signaling
/// the terminal itself); `stop` just asks it to exit on its next poll tick,
/// and the process exiting takes care of the rest for a short-lived CLI
/// command either way.
pub struct InputThread {
    stop: Arc<AtomicBool>,
}

impl InputThread {
    /// Spawn the reader thread, returning both this handle and the receiving
    /// end of the channel it forwards events on.
    pub fn spawn() -> (Self, mpsc::Receiver<Event>) {
        let (tx, rx) = mpsc::channel(INPUT_CHANNEL_CAPACITY);
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            while !thread_stop.load(Ordering::Relaxed) {
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
                        Err(_) => break,
                    },
                    Ok(false) => {}
                    Err(_) => break,
                }
            }
        });
        (Self { stop }, rx)
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

impl Drop for InputThread {
    fn drop(&mut self) {
        self.stop();
    }
}
