//! Bridge crossterm's blocking event reader onto the async world: a
//! dedicated OS thread polls/reads terminal events and forwards them over an
//! unbounded channel, so `supervise_until_shutdown`'s `select!` loop can
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
    pub fn spawn() -> (Self, mpsc::UnboundedReceiver<Event>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            while !thread_stop.load(Ordering::Relaxed) {
                match event::poll(POLL_INTERVAL) {
                    Ok(true) => match event::read() {
                        Ok(ev) => {
                            if tx.send(ev).is_err() {
                                break;
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
