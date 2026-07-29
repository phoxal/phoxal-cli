//! Full-screen ratatui robot-developer session surface.

mod input;
pub(crate) mod log_view;
pub mod render;
pub(crate) mod runtime_view;
mod startup;
pub(crate) mod state;
mod terminal;
mod view_model;
mod visibility;

pub use terminal::{TerminalGuard, install_panic_hook};

use std::io::{self, Stderr};
use std::time::{Duration, Instant};

use crate::Theme;
use crossterm::event::{Event, KeyEventKind};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use tokio::sync::mpsc;

use crate::tui::log_view::LogView;
use crate::tui::render::TitleInfo;
use crate::tui::runtime_view::RuntimeView;
use crate::tui::state::AppState;
use crate::tui::view_model::SessionViewModel;
use phoxal_cli_core::session::TelemetrySnapshot;
use phoxal_cli_core::session::event::{DiagnosticLevel, SessionEvent};
use phoxal_cli_core::session::sanitize_terminal_text;
use phoxal_cli_core::session::{BoardSnapshot, RoutedLogUpdate};

pub struct TuiDisplay {
    theme: Theme,
    title: TitleInfo,
    startup: startup::StartupState,
    state: AppState,
    logs: LogView,
    log_tx: mpsc::Sender<RoutedLogUpdate>,
    log_rx: mpsc::Receiver<RoutedLogUpdate>,
    runtime: RuntimeView,
    telemetry: phoxal_cli_core::session::TelemetrySnapshot,
    activated: Option<Activated>,
}

/// A terminal input request for the root session controller to execute.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DisplayAction {
    None,
    Quit,
    Restart(String),
    JoypadSelect(String),
    JoypadSetEnabled(bool),
    JoypadRescan,
}

const LOG_CHANNEL_CAPACITY: usize = 512;

struct Activated {
    input_thread: input::InputThread,
    input_rx: mpsc::Receiver<Event>,
    terminal: Terminal<CrosstermBackend<Stderr>>,
    last_title_refresh: Instant,
    title_refreshes_remaining: u8,
    _guard: terminal::TerminalGuard,
}

impl Drop for Activated {
    fn drop(&mut self) {
        self.input_thread.stop_and_join();
    }
}

impl TuiDisplay {
    #[must_use]
    pub fn new(theme: Theme, title: TitleInfo) -> Self {
        let (log_tx, log_rx) = mpsc::channel(LOG_CHANNEL_CAPACITY);
        Self {
            theme,
            state: AppState::for_mode(title.mode),
            title,
            startup: startup::StartupState::new(),
            logs: LogView::new(),
            log_tx,
            log_rx,
            runtime: RuntimeView::new(),
            telemetry: phoxal_cli_core::session::TelemetrySnapshot::default(),
            activated: None,
        }
    }

    pub fn set_simulation_info(
        &mut self,
        simulation: Option<&phoxal_cli_core::session::SimulationSessionInfo>,
    ) {
        self.title.simulation_profile = simulation.map(|info| info.profile.clone());
        self.title.simulation_world = simulation.map(|info| info.world.clone());
    }

    pub fn set_bus_endpoint(&mut self, endpoint: String) {
        self.title.bus_endpoint = endpoint;
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
    pub fn log_sender(&self) -> mpsc::Sender<RoutedLogUpdate> {
        self.log_tx.clone()
    }

    pub fn activate(&mut self) -> io::Result<()> {
        if self.activated.is_some() {
            return Ok(());
        }
        let terminal_title = terminal_title(&self.title);
        let guard = terminal::TerminalGuard::enter(&terminal_title)?;
        let backend = CrosstermBackend::new(io::stderr());
        let mut terminal = Terminal::new(backend)?;
        // Alternate-screen entry does not guarantee a blank buffer. Clear
        // before any session preparation can write or the first frame draws.
        terminal.clear()?;
        let _ = terminal::TerminalGuard::set_title(&terminal_title);
        let (input_thread, input_rx) = input::InputThread::spawn();
        self.activated = Some(Activated {
            input_thread,
            input_rx,
            terminal,
            last_title_refresh: Instant::now(),
            title_refreshes_remaining: 5,
            _guard: guard,
        });
        Ok(())
    }

    pub fn redraw(
        &mut self,
        board: &BoardSnapshot,
        telemetry: TelemetrySnapshot,
    ) -> io::Result<()> {
        while let Ok(update) = self.log_rx.try_recv() {
            match update {
                RoutedLogUpdate::ReplaceAll(lines) => self.logs.replace_all(lines),
            }
        }
        self.runtime.observe_board(board);
        self.telemetry = telemetry;
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
                    &render::StartupView {
                        title: &self.title,
                        startup: &self.startup,
                        state: &self.state,
                        telemetry: &self.telemetry,
                        now,
                    },
                );
            } else {
                render::draw(frame, self.theme, &self.title, &mut self.state, &model);
            }
        })?;
        if activated.title_refreshes_remaining > 0
            && activated.last_title_refresh.elapsed() >= Duration::from_secs(1)
        {
            let _ = terminal::TerminalGuard::set_title(&terminal_title(&self.title));
            activated.last_title_refresh = Instant::now();
            activated.title_refreshes_remaining -= 1;
        }
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
                None => Err(io::Error::other(
                    "terminal input reader stopped unexpectedly",
                )),
            },
        }
    }

    pub fn handle_input(&mut self, event: Event, board: &BoardSnapshot) -> DisplayAction {
        if needs_full_clear(&event) {
            if let Some(activated) = &mut self.activated {
                let _ = activated.terminal.clear();
                let _ = terminal::TerminalGuard::set_title(&terminal_title(&self.title));
                activated.last_title_refresh = Instant::now();
                activated.title_refreshes_remaining = 2;
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
        self.state.sync(&model);
        self.state.handle_key(key, &model)
    }
}

fn needs_full_clear(event: &Event) -> bool {
    matches!(event, Event::Resize(_, _) | Event::FocusGained)
}

fn terminal_title(title: &TitleInfo) -> String {
    sanitize_terminal_text(&format!("phoxal - {} - {}", title.robot, title.namespace))
}

#[cfg(test)]
mod tests {
    use std::time::SystemTime;

    use super::*;
    use crate::ColorCapability;
    use phoxal_cli_core::session::{LogSeverity, LogSource, RoutedLogLine};

    fn title() -> TitleInfo {
        TitleInfo {
            robot: "rover-01".to_string(),
            namespace: "dev".to_string(),
            train: "0.36.0".to_string(),
            manifest: "./robot.yaml".to_string(),
            mode: phoxal_cli_core::session::SessionMode::Run,
            bus_endpoint: "tcp/localhost:7447".to_string(),
            simulation_profile: None,
            simulation_world: None,
            started_at: SystemTime::UNIX_EPOCH,
            started_instant: Instant::now(),
        }
    }

    #[test]
    fn dormant_display_syncs_without_touching_terminal() {
        let mut display = TuiDisplay::new(Theme::new(ColorCapability::None), title());
        assert!(
            display
                .redraw(&BoardSnapshot::default(), TelemetrySnapshot::default())
                .is_ok()
        );
        assert!(display.activated.is_none());
    }

    #[test]
    fn late_webots_snapshot_updates_session_information() {
        let mut display = TuiDisplay::new(Theme::new(ColorCapability::None), title());
        display.set_simulation_info(Some(&phoxal_cli_core::session::SimulationSessionInfo {
            profile: "webots".to_string(),
            world: "worlds/default.wbt".to_string(),
        }));

        assert_eq!(display.title.simulation_profile.as_deref(), Some("webots"));
        assert_eq!(
            display.title.simulation_world.as_deref(),
            Some("worlds/default.wbt")
        );
    }

    #[test]
    fn routed_logs_are_available_before_activation() {
        let display = TuiDisplay::new(Theme::new(ColorCapability::None), title());
        assert!(
            display
                .log_sender()
                .try_send(RoutedLogUpdate::ReplaceAll(vec![RoutedLogLine {
                    participant: "drive".to_string(),
                    source: LogSource::Bus,
                    severity: LogSeverity::Info,
                    text: "hello".to_string(),
                    event_time: SystemTime::UNIX_EPOCH,
                    scope: None,
                }]))
                .is_ok()
        );
    }

    #[test]
    fn errors_open_global_logs_instead_of_a_diagnostics_page() {
        let mut display = TuiDisplay::new(Theme::new(ColorCapability::None), title());
        display.apply_session_event(&SessionEvent::Diagnostic {
            source: phoxal_cli_core::session::event::DiagnosticSource::Dependency,
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

    #[test]
    fn terminal_title_names_the_robot_and_namespace() {
        assert_eq!(terminal_title(&title()), "phoxal - rover-01 - dev");
        let mut unsafe_title = title();
        unsafe_title.robot = "rover\u{7}spoof".to_string();
        assert_eq!(terminal_title(&unsafe_title), "phoxal - rover spoof - dev");
    }
}
