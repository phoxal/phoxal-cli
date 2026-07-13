//! The full-screen ratatui TUI - the ONLY interactive renderer under
//! [`crate::output_mode::OutputMode::Rich`] on a real TTY (see
//! [`crate::session::controller::SessionController`]). This module builds the
//! SHELL: the grouped navigator, the Overview-first right pane, per-runtime
//! detail tabs (Overview/Logs/a hardcoded bespoke third tab), terminal
//! ownership, and the insertion points a later slice fills in (top-bar sim
//! clock/host resource meters, per-runtime CPU/RAM, the Traffic/Devices/
//! Resources tab bodies).
//!
//! # Architecture
//!
//! [`TuiDisplay`] is owned and driven directly by `SessionController` - not
//! the supervisor, which knows nothing about ratatui, input, or terminal
//! activation at all. The controller calls [`TuiDisplay::activate`]
//! immediately on construction (before preparation even begins, so there is
//! ONE interactive surface for the whole session), then redraws it on its own
//! ticker and bridges its input to `SessionEvent`/`DisplayAction` handling -
//! see that module's docs for the full loop shape.
//!
//! - **Input** is bridged from crossterm's blocking `event::read()` onto a
//!   channel by a small dedicated OS thread ([`input::InputThread`]),
//!   spawned lazily in [`TuiDisplay::activate`]; the controller's `select!`
//!   arm just does a cancel-safe `.recv()` on that channel.
//! - **Log routing** ([`logs::LogRouter`]) is wired the instant a
//!   `TuiDisplay` exists (`BoardBackend::set_log_sink`), independent of
//!   `activate` - so by the time the alternate screen actually opens, the
//!   scrollback is not empty.

mod color;
mod groups;
mod input;
mod logs;
mod render;
mod state;
mod terminal;

pub(crate) use terminal::TerminalGuard;

use std::io::{self, Stderr};

use crossterm::event::{Event, KeyEventKind};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use tokio::sync::mpsc;

use crate::display::DisplayAction;
use crate::supervisor::{BoardSnapshot, RoutedLogLine};
use crate::telemetry::TelemetryBackend;
use crate::theme::Theme;

pub use logs::LogRouter;
pub use render::TitleInfo;
pub use state::AppState;

/// The full-screen TUI display. Constructed dormant (no terminal mutation) by
/// [`TuiDisplay::new`]; [`TuiDisplay::activate`] is what actually enters raw
/// mode/the alternate screen and starts reading input - `SessionController`
/// calls it immediately on construction, so this is the ONE interactive
/// surface for the whole session, active before preparation even begins.
pub struct TuiDisplay {
    theme: Theme,
    title: TitleInfo,
    state: AppState,
    logs: LogRouter,
    log_tx: mpsc::UnboundedSender<RoutedLogLine>,
    log_rx: mpsc::UnboundedReceiver<RoutedLogLine>,
    activated: Option<Activated>,
}

struct Activated {
    _guard: terminal::TerminalGuard,
    terminal: Terminal<CrosstermBackend<Stderr>>,
    _input_thread: input::InputThread,
    input_rx: mpsc::UnboundedReceiver<Event>,
}

impl std::fmt::Debug for TuiDisplay {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TuiDisplay")
            .field("activated", &self.activated.is_some())
            .finish_non_exhaustive()
    }
}

impl TuiDisplay {
    #[must_use]
    pub fn new(theme: Theme, title: TitleInfo) -> Self {
        let (log_tx, log_rx) = mpsc::unbounded_channel();
        Self {
            theme,
            title,
            state: AppState::new(),
            logs: LogRouter::new(),
            log_tx,
            log_rx,
            activated: None,
        }
    }

    /// The sender end for [`crate::supervisor::BoardBackend::set_log_sink`] -
    /// stable for this `TuiDisplay`'s whole lifetime, independent of
    /// [`Self::activate`], so log routing starts the instant this display
    /// exists.
    #[must_use]
    pub fn log_sender(&self) -> mpsc::UnboundedSender<RoutedLogLine> {
        self.log_tx.clone()
    }

    /// Enter raw mode + the alternate screen and start reading input. Only
    /// call once the caller has already confirmed stderr is a real terminal
    /// under `OutputMode::Rich` - see `terminal::TerminalGuard::should_use_terminal`,
    /// which `SessionController::new` checks before ever constructing a
    /// `TuiDisplay`.
    pub fn activate(&mut self) -> io::Result<()> {
        if self.activated.is_some() {
            return Ok(());
        }
        let guard = terminal::TerminalGuard::enter()?;
        let backend = CrosstermBackend::new(io::stderr());
        let terminal = Terminal::new(backend)?;
        let (input_thread, input_rx) = input::InputThread::spawn();
        self.activated = Some(Activated {
            _guard: guard,
            terminal,
            _input_thread: input_thread,
            input_rx,
        });
        Ok(())
    }

    /// Drain every routed log line queued since the last redraw, then draw
    /// one frame if activated. Draining happens regardless of activation so
    /// scrollback keeps accumulating even before the alternate screen opens.
    /// `telemetry` is snapshotted once here and both fed into the
    /// per-runtime history the reserved `RuntimeLogState` slots keep
    /// (`LogRouter::record_telemetry`) and passed straight through to
    /// `render::draw` for the non-historical readouts (sim clock, host meter,
    /// per-participant CPU/RAM, joypad selection).
    pub fn redraw(&mut self, board: &BoardSnapshot, telemetry: &TelemetryBackend) {
        while let Ok(line) = self.log_rx.try_recv() {
            self.logs.record(line);
        }
        self.state.sync(board);
        let snapshot = telemetry.snapshot();
        self.logs.record_telemetry(&snapshot);
        let Some(activated) = &mut self.activated else {
            return;
        };
        let theme = self.theme;
        let title = &self.title;
        let state = &self.state;
        let logs = &mut self.logs;
        let _ = activated.terminal.draw(|frame| {
            render::draw(frame, theme, title, board, logs, state, &snapshot);
        });
    }

    /// Cancel-safe: `.recv()` on an `mpsc::UnboundedReceiver` is documented
    /// cancel-safe, so this may be dropped mid-await by a competing
    /// `select!` branch without losing an already-buffered event.
    pub async fn next_input(&mut self) -> Option<Event> {
        let activated = self.activated.as_mut()?;
        match activated.input_rx.recv().await {
            Some(event) => Some(event),
            // A terminal read error ends the bridge thread and closes this
            // channel. Stay pending instead of returning `None` on every
            // supervisor select pass and spinning a CPU core.
            None => std::future::pending().await,
        }
    }

    pub fn handle_input(
        &mut self,
        event: Event,
        board: &BoardSnapshot,
        telemetry: &TelemetryBackend,
    ) -> DisplayAction {
        let Event::Key(key) = event else {
            if let Event::Resize(_, _) = event {
                // Nothing to recompute eagerly - the next redraw tick reads
                // the terminal's own current size via `Frame::area()`.
            }
            return DisplayAction::None;
        };
        // crossterm reports both press and release on platforms that
        // distinguish them (Windows, some terminals under the kitty
        // keyboard protocol); only act on press to avoid double-handling.
        if key.kind != KeyEventKind::Press {
            return DisplayAction::None;
        }
        self.state.handle_key(key, board, &telemetry.snapshot())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::ColorCapability;

    fn title() -> TitleInfo {
        TitleInfo {
            robot: "rover-01".to_string(),
            channel: "dev".to_string(),
            mode: "run",
        }
    }

    #[test]
    fn a_dormant_display_never_touches_the_terminal_but_still_syncs_state() {
        let mut display = TuiDisplay::new(Theme::new(ColorCapability::None), title());
        let mut board = BoardSnapshot::default();
        board.participants.insert(
            "drive".to_string(),
            crate::supervisor::ParticipantStatus::new(
                "drive",
                crate::participant_kind::ParticipantKind::Service,
                crate::supervisor::ParticipantState::Ready,
            ),
        );
        // Must not panic even though `activate` was never called.
        display.redraw(&board, &TelemetryBackend::default());
        assert!(display.activated.is_none());
    }

    #[test]
    fn log_sender_is_stable_before_activation() {
        let display = TuiDisplay::new(Theme::new(ColorCapability::None), title());
        let sender = display.log_sender();
        assert!(
            sender
                .send(RoutedLogLine {
                    participant: "drive".to_string(),
                    source: crate::supervisor::LogSource::Bus,
                    text: "hello".to_string(),
                })
                .is_ok()
        );
    }
}
