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
//! - **Log routing** ([`crate::stores::log_store::LogStore`]) is wired the
//!   instant a `TuiDisplay` exists (`BoardBackend::set_log_sink`),
//!   independent of `activate` - so by the time the alternate screen
//!   actually opens, the scrollback is not empty.
//! - **The startup surface** ([`startup::StartupState`]) is fed every
//!   [`crate::session::event::SessionEvent`] the controller applies
//!   ([`TuiDisplay::apply_session_event`]) and renders in place of the
//!   runtime navigator until the session reaches `Running` -
//!   see that module's docs for the pure model.

mod color;
mod diagnostics;
mod groups;
mod input;
mod panel;
mod render;
mod startup;
mod state;
mod terminal;

pub(crate) use terminal::TerminalGuard;

use std::io::{self, Stderr};
use std::time::Instant;

use crossterm::event::{Event, KeyEventKind};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use tokio::sync::mpsc;

use crate::display::DisplayAction;
use crate::identity::IdentitySummary;
use crate::session::event::{DiagnosticLevel, SessionEvent};
use crate::stores::log_store::LogStore;
use crate::stores::runtime_store::RuntimeStore;
use crate::supervisor::{BoardSnapshot, RoutedLogLine};
use crate::telemetry::TelemetryBackend;
use crate::theme::Theme;

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
    identity: Option<IdentitySummary>,
    startup: startup::StartupState,
    diagnostics: diagnostics::DiagnosticsStore,
    started_at: Instant,
    state: AppState,
    logs: LogStore,
    log_tx: mpsc::Sender<RoutedLogLine>,
    log_rx: mpsc::Receiver<RoutedLogLine>,
    /// Session-only participant metadata (finding A5) - empty until the
    /// caller's launch plan is known, via [`Self::set_runtime_store`]. An
    /// empty store still renders correctly: every lookup is `None`/`0`, the
    /// same "not wired yet" shape the Overview/Traffic panels already handle.
    runtime: RuntimeStore,
    activated: Option<Activated>,
}

/// Overflow policy for the routed-log channel: drop-newest. The bounded
/// `LogStore` ring is the real backstop against unbounded memory growth; this
/// capacity only needs to absorb a burst between redraws, not hold a whole
/// session's history.
const LOG_CHANNEL_CAPACITY: usize = 512;

struct Activated {
    _guard: terminal::TerminalGuard,
    terminal: Terminal<CrosstermBackend<Stderr>>,
    input_thread: input::InputThread,
    input_rx: mpsc::Receiver<Event>,
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
    pub fn new(theme: Theme, title: TitleInfo, identity: Option<IdentitySummary>) -> Self {
        let (log_tx, log_rx) = mpsc::channel(LOG_CHANNEL_CAPACITY);
        Self {
            theme,
            title,
            identity,
            startup: startup::StartupState::new(),
            diagnostics: diagnostics::DiagnosticsStore::default(),
            started_at: Instant::now(),
            state: AppState::new(),
            logs: LogStore::new(),
            log_tx,
            log_rx,
            runtime: RuntimeStore::new(),
            activated: None,
        }
    }

    /// Install this session's launch-time participant metadata (finding A5),
    /// called once by `SessionController` right before supervision starts,
    /// alongside `BoardBackend::set_log_sink`. Replaces any previous store
    /// wholesale rather than merging, since a session builds exactly one
    /// launch plan.
    pub fn set_runtime_store(&mut self, runtime: RuntimeStore) {
        self.runtime = runtime;
    }

    /// Fold one [`SessionEvent`] the controller applies into the startup
    /// model - see `startup::StartupState::apply_event`. Called from
    /// `SessionController::forward_to_renderer` for every event the
    /// controller itself applies (phase/diagnostic/session transitions), so
    /// the NEXT [`Self::redraw`] reflects it.
    pub fn apply_session_event(&mut self, event: &SessionEvent) {
        let during_startup = self.startup.show_startup_surface();
        if let SessionEvent::Diagnostic {
            source,
            level,
            message,
        } = event
        {
            self.diagnostics.record(
                self.started_at.elapsed(),
                source.clone(),
                *level,
                message.clone(),
            );
            if during_startup && *level == DiagnosticLevel::Error {
                self.state.open_diagnostics();
            }
        }
        self.startup.apply_event(event);
    }

    /// The sender end for [`crate::supervisor::BoardBackend::set_log_sink`] -
    /// stable for this `TuiDisplay`'s whole lifetime, independent of
    /// [`Self::activate`], so log routing starts the instant this display
    /// exists.
    #[must_use]
    pub fn log_sender(&self) -> mpsc::Sender<RoutedLogLine> {
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
            input_thread,
            input_rx,
        });
        Ok(())
    }

    /// Drain every routed log line queued since the last redraw, then draw
    /// one frame if activated. Draining happens regardless of activation so
    /// scrollback keeps accumulating even before the alternate screen opens.
    /// `telemetry` is snapshotted once here - the snapshot's samples are
    /// already timestamped at their actual bus-receive time by
    /// `TelemetryBackend`/`stores::telemetry_store::TelemetryStore` (Wave
    /// C2), so `now` (this redraw's own instant) is only needed to compare
    /// against those receive times for staleness, never to re-stamp a
    /// sample. Renders the dedicated startup surface (`render::draw_startup`)
    /// instead of the runtime navigator while
    /// `self.startup.show_startup_surface()` is true - see `tui::startup`'s
    /// module docs. Warnings and errors are retained separately in the
    /// session-wide Diagnostics tab, never printed over either surface.
    /// Draw one frame if activated. An `Err` here is a genuine terminal I/O
    /// failure (e.g. the tty went away underneath the session) - the caller
    /// (`SessionController`) treats it as a session failure rather than
    /// silently leaving the screen stale forever.
    pub fn redraw(
        &mut self,
        board: &BoardSnapshot,
        telemetry: &TelemetryBackend,
    ) -> io::Result<()> {
        while let Ok(line) = self.log_rx.try_recv() {
            self.logs.record(line);
        }
        self.state.sync(board);
        self.runtime.observe_board(board);
        let snapshot = telemetry.snapshot();
        let now = Instant::now();
        let Some(activated) = &mut self.activated else {
            return Ok(());
        };
        let theme = self.theme;
        let title = &self.title;
        let identity = self.identity.as_ref();
        let startup = &self.startup;
        let diagnostics = &self.diagnostics;
        let state = &self.state;
        let logs = &mut self.logs;
        let runtime = &self.runtime;
        activated.terminal.draw(|frame| {
            if startup.show_startup_surface() && state.view != state::View::Diagnostics {
                render::draw_startup(frame, theme, title, identity, startup);
            } else {
                render::draw(
                    frame,
                    theme,
                    title,
                    board,
                    logs,
                    state,
                    &snapshot,
                    runtime,
                    now,
                    diagnostics,
                );
            }
        })?;
        Ok(())
    }

    /// Cancel-safe: `.recv()` on an `mpsc::Receiver` is documented
    /// cancel-safe, so this may be dropped mid-await by a competing
    /// `select!` branch without losing an already-buffered event.
    ///
    /// `Ok(None)` means no more input will ever arrive but nothing is
    /// wrong (the reader thread stopped cleanly - e.g. mid-teardown); the
    /// caller should stay pending rather than spin. `Err` means the reader
    /// thread ended because `event::poll`/`event::read` itself failed (the
    /// tty went away underneath the session) - a real failure the caller
    /// should surface, not swallow.
    pub async fn next_input(&mut self) -> io::Result<Option<Event>> {
        let Some(activated) = self.activated.as_mut() else {
            return Ok(None);
        };
        match activated.input_rx.recv().await {
            Some(event) => Ok(Some(event)),
            None => match activated.input_thread.take_error() {
                Some(error) => Err(error),
                None => std::future::pending().await,
            },
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
        let mut display = TuiDisplay::new(Theme::new(ColorCapability::None), title(), None);
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
        assert!(display.redraw(&board, &TelemetryBackend::default()).is_ok());
        assert!(display.activated.is_none());
    }

    #[test]
    fn log_sender_is_stable_before_activation() {
        let display = TuiDisplay::new(Theme::new(ColorCapability::None), title(), None);
        let sender = display.log_sender();
        assert!(
            sender
                .try_send(RoutedLogLine {
                    participant: "drive".to_string(),
                    source: crate::supervisor::LogSource::Bus,
                    text: "hello".to_string(),
                })
                .is_ok()
        );
    }

    #[test]
    fn warning_is_retained_without_interrupting_startup() {
        let mut display = TuiDisplay::new(Theme::new(ColorCapability::None), title(), None);
        display.apply_session_event(&SessionEvent::Diagnostic {
            source: crate::session::event::DiagnosticSource::Cli,
            level: DiagnosticLevel::Warn,
            message: "connection retrying".to_string(),
        });

        assert_eq!(display.diagnostics.len(), 1);
        assert_eq!(display.state.view, state::View::Home);
        assert!(display.startup.show_startup_surface());
    }

    #[test]
    fn startup_error_opens_diagnostics_while_info_is_ignored() {
        let mut display = TuiDisplay::new(Theme::new(ColorCapability::None), title(), None);
        display.apply_session_event(&SessionEvent::Diagnostic {
            source: crate::session::event::DiagnosticSource::Dependency,
            level: DiagnosticLevel::Info,
            message: "staged simulation".to_string(),
        });
        assert!(display.diagnostics.is_empty());

        display.apply_session_event(&SessionEvent::Diagnostic {
            source: crate::session::event::DiagnosticSource::Dependency,
            level: DiagnosticLevel::Error,
            message: "build failed".to_string(),
        });
        assert_eq!(display.diagnostics.error_count(), 1);
        assert_eq!(display.state.view, state::View::Diagnostics);
    }
}
