//! Full-screen robot-developer session surface.

mod color;
mod input;
mod render;
mod startup;
mod state;
mod terminal;
mod view_model;
mod visibility;

pub(crate) use terminal::TerminalGuard;

use std::collections::BTreeMap;
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
use crate::tui::view_model::SessionViewModel;

pub use render::TitleInfo;
pub use state::AppState;

pub struct TuiDisplay {
    theme: Theme,
    title: TitleInfo,
    identity: Option<IdentitySummary>,
    startup: startup::StartupState,
    state: AppState,
    logs: LogStore,
    log_tx: mpsc::Sender<RoutedLogLine>,
    log_rx: mpsc::Receiver<RoutedLogLine>,
    runtime: RuntimeStore,
    telemetry: crate::telemetry::TelemetrySnapshot,
    heartbeats: BTreeMap<String, Instant>,
    activated: Option<Activated>,
}

const LOG_CHANNEL_CAPACITY: usize = 512;

struct Activated {
    _guard: terminal::TerminalGuard,
    terminal: Terminal<CrosstermBackend<Stderr>>,
    input_thread: input::InputThread,
    input_rx: mpsc::Receiver<Event>,
}

impl std::fmt::Debug for TuiDisplay {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TuiDisplay")
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
            state: AppState::for_mode(title.mode),
            title,
            identity,
            startup: startup::StartupState::new(),
            logs: LogStore::new(),
            log_tx,
            log_rx,
            runtime: RuntimeStore::new(),
            telemetry: crate::telemetry::TelemetrySnapshot::default(),
            heartbeats: BTreeMap::new(),
            activated: None,
        }
    }

    pub fn set_runtime_store(&mut self, runtime: RuntimeStore) {
        self.runtime = runtime;
    }

    pub fn set_heartbeats(&mut self, heartbeats: BTreeMap<String, Instant>) {
        self.heartbeats = heartbeats;
    }

    pub fn apply_session_event(&mut self, event: &SessionEvent) {
        if let SessionEvent::Diagnostic {
            source,
            level,
            message,
        } = event
        {
            self.logs
                .record_diagnostic(format!("phoxal-cli/{source:?}"), *level, message.clone());
            if *level == DiagnosticLevel::Error {
                self.state.open_logs_for_error();
            }
        }
        self.startup.apply_event(event);
    }

    #[must_use]
    pub fn log_sender(&self) -> mpsc::Sender<RoutedLogLine> {
        self.log_tx.clone()
    }

    pub fn activate(&mut self) -> io::Result<()> {
        if self.activated.is_some() {
            return Ok(());
        }
        let guard = terminal::TerminalGuard::enter()?;
        let backend = CrosstermBackend::new(io::stderr());
        let mut terminal = Terminal::new(backend)?;
        // Alternate-screen entry does not guarantee a blank buffer. Clear
        // before any session preparation can write or the first frame draws.
        terminal.clear()?;
        let (input_thread, input_rx) = input::InputThread::spawn();
        self.activated = Some(Activated {
            _guard: guard,
            terminal,
            input_thread,
            input_rx,
        });
        Ok(())
    }

    pub fn redraw(
        &mut self,
        board: &BoardSnapshot,
        telemetry: &TelemetryBackend,
    ) -> io::Result<()> {
        while let Ok(line) = self.log_rx.try_recv() {
            self.logs.record(line);
        }
        self.runtime.observe_board(board, &self.heartbeats);
        self.telemetry = telemetry.snapshot();
        let now = Instant::now();
        let model = SessionViewModel::new(board, &self.logs, &self.runtime, &self.telemetry, now);
        self.state.sync(&model);
        let Some(activated) = &mut self.activated else {
            return Ok(());
        };
        let show_startup = self.startup.show_startup_surface()
            && self.state.page == state::Page::Overview
            && !self.state.show_help
            && !self.state.show_info;
        activated.terminal.draw(|frame| {
            if show_startup {
                render::draw_startup(
                    frame,
                    self.theme,
                    &self.title,
                    self.identity.as_ref(),
                    &self.startup,
                );
            } else {
                render::draw(
                    frame,
                    self.theme,
                    &self.title,
                    self.identity.as_ref(),
                    &self.state,
                    &model,
                );
            }
        })?;
        Ok(())
    }

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
        _telemetry: &TelemetryBackend,
    ) -> DisplayAction {
        if needs_full_clear(&event) {
            if let Some(activated) = &mut self.activated {
                let _ = activated.terminal.clear();
            }
            return DisplayAction::None;
        }
        let Event::Key(key) = event else {
            return DisplayAction::None;
        };
        if key.kind != KeyEventKind::Press {
            return DisplayAction::None;
        }
        let model = SessionViewModel::new(
            board,
            &self.logs,
            &self.runtime,
            &self.telemetry,
            Instant::now(),
        );
        self.state.handle_key(key, &model)
    }
}

fn needs_full_clear(event: &Event) -> bool {
    matches!(event, Event::Resize(_, _) | Event::FocusGained)
}

#[cfg(test)]
mod tests {
    use std::time::SystemTime;

    use super::*;
    use crate::supervisor::{LogSeverity, LogSource};
    use crate::theme::ColorCapability;

    fn title() -> TitleInfo {
        TitleInfo {
            robot: "rover-01".to_string(),
            channel: "dev".to_string(),
            mode: "run",
            bus_endpoint: "tcp/localhost:7447".to_string(),
            started_at: SystemTime::UNIX_EPOCH,
        }
    }

    #[test]
    fn dormant_display_syncs_without_touching_terminal() {
        let mut display = TuiDisplay::new(Theme::new(ColorCapability::None), title(), None);
        assert!(
            display
                .redraw(&BoardSnapshot::default(), &TelemetryBackend::default())
                .is_ok()
        );
        assert!(display.activated.is_none());
    }

    #[test]
    fn routed_logs_are_available_before_activation() {
        let display = TuiDisplay::new(Theme::new(ColorCapability::None), title(), None);
        assert!(
            display
                .log_sender()
                .try_send(RoutedLogLine {
                    participant: "drive".to_string(),
                    source: LogSource::Bus,
                    severity: LogSeverity::Info,
                    text: "hello".to_string(),
                })
                .is_ok()
        );
    }

    #[test]
    fn errors_open_global_logs_instead_of_a_diagnostics_page() {
        let mut display = TuiDisplay::new(Theme::new(ColorCapability::None), title(), None);
        display.apply_session_event(&SessionEvent::Diagnostic {
            source: crate::session::event::DiagnosticSource::Dependency,
            level: DiagnosticLevel::Error,
            message: "build failed".to_string(),
        });
        assert_eq!(display.state.page, state::Page::Logs);
        assert_eq!(display.logs.lines().count(), 1);
    }

    #[test]
    fn resize_and_focus_resume_require_a_full_clear() {
        assert!(needs_full_clear(&Event::Resize(80, 24)));
        assert!(needs_full_clear(&Event::FocusGained));
        assert!(!needs_full_clear(&Event::FocusLost));
    }
}
