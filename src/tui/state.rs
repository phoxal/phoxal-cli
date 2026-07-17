//! Pure five-page session navigation and input handling.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::display::DisplayAction;
use crate::supervisor::LogSeverity;
use crate::tui::view_model::SessionViewModel;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Page {
    #[default]
    Overview,
    Runtimes,
    Logs,
    Bus,
    Input,
}

impl Page {
    pub const ALL: [Self; 5] = [
        Self::Overview,
        Self::Runtimes,
        Self::Logs,
        Self::Bus,
        Self::Input,
    ];

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Overview => "Overview",
            Self::Runtimes => "Runtimes",
            Self::Logs => "Logs",
            Self::Bus => "Bus",
            Self::Input => "Input",
        }
    }

    #[must_use]
    pub fn index(self) -> usize {
        Self::ALL.iter().position(|page| *page == self).unwrap_or(0)
    }

    fn offset(self, delta: isize) -> Self {
        let len = Self::ALL.len() as isize;
        Self::ALL[((self.index() as isize + delta).rem_euclid(len)) as usize]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SeverityFilter {
    #[default]
    All,
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

impl SeverityFilter {
    fn cycle(self) -> Self {
        match self {
            Self::All => Self::Error,
            Self::Error => Self::Warn,
            Self::Warn => Self::Info,
            Self::Info => Self::Debug,
            Self::Debug => Self::Trace,
            Self::Trace => Self::All,
        }
    }

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Error => "error",
            Self::Warn => "warn",
            Self::Info => "info",
            Self::Debug => "debug",
            Self::Trace => "trace",
        }
    }

    #[must_use]
    pub const fn matches(self, severity: LogSeverity) -> bool {
        match self {
            Self::All => true,
            Self::Error => matches!(severity, LogSeverity::Error),
            Self::Warn => matches!(severity, LogSeverity::Warn),
            Self::Info => matches!(severity, LogSeverity::Info),
            Self::Debug => matches!(severity, LogSeverity::Debug),
            Self::Trace => matches!(severity, LogSeverity::Trace),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BusSort {
    #[default]
    Rate,
    Topic,
    Producer,
}

impl BusSort {
    fn cycle(self) -> Self {
        match self {
            Self::Rate => Self::Topic,
            Self::Topic => Self::Producer,
            Self::Producer => Self::Rate,
        }
    }

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Rate => "rate",
            Self::Topic => "topic",
            Self::Producer => "producer",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Editing {
    LogText,
    LogRuntime,
    Bus,
}

#[derive(Debug, Clone)]
pub struct AppState {
    pub page: Page,
    pub runtime_cursor: usize,
    pub runtime_detail: bool,
    pub log_text_filter: String,
    pub log_runtime_filter: String,
    pub log_severity: SeverityFilter,
    pub log_scroll: usize,
    pub log_follow: bool,
    pub bus_filter: String,
    pub bus_sort: BusSort,
    pub bus_scroll: usize,
    pub bus_show_internal: bool,
    pub input_cursor: usize,
    pub show_help: bool,
    pub show_info: bool,
    editing: Option<Editing>,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            page: Page::Overview,
            runtime_cursor: 0,
            runtime_detail: false,
            log_text_filter: String::new(),
            log_runtime_filter: String::new(),
            log_severity: SeverityFilter::All,
            log_scroll: 0,
            log_follow: true,
            bus_filter: String::new(),
            bus_sort: BusSort::Rate,
            bus_scroll: 0,
            bus_show_internal: false,
            input_cursor: 0,
            show_help: false,
            show_info: false,
            editing: None,
        }
    }
}

impl AppState {
    #[must_use]
    pub fn for_mode(_mode: &str) -> Self {
        Self::default()
    }

    pub fn sync(&mut self, model: &SessionViewModel<'_>) {
        self.runtime_cursor = self
            .runtime_cursor
            .min(model.runtimes.len().saturating_sub(1));
        let device_count = model
            .telemetry
            .joypad
            .as_ref()
            .map_or(0, |joypad| joypad.value.available.len());
        self.input_cursor = self.input_cursor.min(device_count.saturating_sub(1));
        if self.log_follow {
            self.log_scroll = 0;
        }
    }

    #[must_use]
    pub fn editing_label(&self) -> Option<&'static str> {
        self.editing.map(|editing| match editing {
            Editing::LogText => "log text",
            Editing::LogRuntime => "runtime",
            Editing::Bus => "bus",
        })
    }

    pub fn open_logs_for_error(&mut self) {
        self.page = Page::Logs;
        self.log_severity = SeverityFilter::Error;
    }

    pub fn handle_key(&mut self, key: KeyEvent, model: &SessionViewModel<'_>) -> DisplayAction {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return DisplayAction::Quit;
        }
        if self.editing.is_some() {
            return self.handle_editing(key);
        }
        match key.code {
            KeyCode::Char('?') => {
                self.show_help = !self.show_help;
                self.show_info = false;
                return DisplayAction::None;
            }
            KeyCode::Char('i') => {
                self.show_info = !self.show_info;
                self.show_help = false;
                return DisplayAction::None;
            }
            KeyCode::Esc if self.show_help || self.show_info => {
                self.show_help = false;
                self.show_info = false;
                return DisplayAction::None;
            }
            KeyCode::Char('q') => return DisplayAction::Quit,
            KeyCode::Char('1') if !self.show_help && !self.show_info => {
                return self.set_page(Page::Overview);
            }
            KeyCode::Char('2') if !self.show_help && !self.show_info => {
                return self.set_page(Page::Runtimes);
            }
            KeyCode::Char('3') if !self.show_help && !self.show_info => {
                return self.set_page(Page::Logs);
            }
            KeyCode::Char('4') if !self.show_help && !self.show_info => {
                return self.set_page(Page::Bus);
            }
            KeyCode::Char('5') if !self.show_help && !self.show_info => {
                return self.set_page(Page::Input);
            }
            KeyCode::Left if !self.runtime_detail && !self.show_help && !self.show_info => {
                return self.set_page(self.page.offset(-1));
            }
            KeyCode::Right if !self.runtime_detail && !self.show_help && !self.show_info => {
                return self.set_page(self.page.offset(1));
            }
            _ => {}
        }
        if self.show_help || self.show_info {
            return DisplayAction::None;
        }

        match self.page {
            Page::Overview => DisplayAction::None,
            Page::Runtimes => self.handle_runtimes(key, model),
            Page::Logs => self.handle_logs(key),
            Page::Bus => self.handle_bus(key),
            Page::Input => self.handle_input_page(key, model),
        }
    }

    fn set_page(&mut self, page: Page) -> DisplayAction {
        let changed = self.page != page;
        self.page = page;
        self.runtime_detail = false;
        if changed && page == Page::Input {
            DisplayAction::JoypadRescan
        } else {
            DisplayAction::None
        }
    }

    fn handle_editing(&mut self, key: KeyEvent) -> DisplayAction {
        match key.code {
            KeyCode::Esc | KeyCode::Enter => self.editing = None,
            KeyCode::Backspace => {
                self.active_filter_mut().pop();
            }
            KeyCode::Char(character)
                if !key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT) =>
            {
                self.active_filter_mut().push(character);
            }
            _ => {}
        }
        DisplayAction::None
    }

    fn active_filter_mut(&mut self) -> &mut String {
        match self.editing.expect("filter editing is active") {
            Editing::LogText => &mut self.log_text_filter,
            Editing::LogRuntime => &mut self.log_runtime_filter,
            Editing::Bus => &mut self.bus_filter,
        }
    }

    fn handle_runtimes(&mut self, key: KeyEvent, model: &SessionViewModel<'_>) -> DisplayAction {
        match key.code {
            KeyCode::Esc if self.runtime_detail => self.runtime_detail = false,
            KeyCode::Enter if !model.runtimes.is_empty() => self.runtime_detail = true,
            KeyCode::Up => self.runtime_cursor = self.runtime_cursor.saturating_sub(1),
            KeyCode::Down => {
                self.runtime_cursor =
                    (self.runtime_cursor + 1).min(model.runtimes.len().saturating_sub(1));
            }
            KeyCode::Char('r') => {
                if let Some(status) = model.runtimes.get(self.runtime_cursor) {
                    return DisplayAction::Restart(status.id.clone());
                }
            }
            KeyCode::Char('l') => {
                if let Some(status) = model.runtimes.get(self.runtime_cursor) {
                    self.log_runtime_filter.clone_from(&status.id);
                    self.page = Page::Logs;
                    self.runtime_detail = false;
                }
            }
            _ => {}
        }
        DisplayAction::None
    }

    fn handle_logs(&mut self, key: KeyEvent) -> DisplayAction {
        match key.code {
            KeyCode::Char('/') => self.editing = Some(Editing::LogText),
            KeyCode::Char('f') => self.editing = Some(Editing::LogRuntime),
            KeyCode::Char('s') => self.log_severity = self.log_severity.cycle(),
            KeyCode::Char(' ') => {
                self.log_follow = !self.log_follow;
                if self.log_follow {
                    self.log_scroll = 0;
                }
            }
            KeyCode::Up => {
                self.log_follow = false;
                self.log_scroll = self.log_scroll.saturating_add(1);
            }
            KeyCode::Down => self.log_scroll = self.log_scroll.saturating_sub(1),
            KeyCode::End => {
                self.log_scroll = 0;
                self.log_follow = true;
            }
            _ => {}
        }
        DisplayAction::None
    }

    fn handle_bus(&mut self, key: KeyEvent) -> DisplayAction {
        match key.code {
            KeyCode::Char('/') => self.editing = Some(Editing::Bus),
            KeyCode::Char('s') => self.bus_sort = self.bus_sort.cycle(),
            KeyCode::Char('a') => self.bus_show_internal = !self.bus_show_internal,
            KeyCode::Up => self.bus_scroll = self.bus_scroll.saturating_add(1),
            KeyCode::Down => self.bus_scroll = self.bus_scroll.saturating_sub(1),
            _ => {}
        }
        DisplayAction::None
    }

    fn handle_input_page(&mut self, key: KeyEvent, model: &SessionViewModel<'_>) -> DisplayAction {
        let devices = model
            .telemetry
            .joypad
            .as_ref()
            .map(|joypad| joypad.value.available.as_slice())
            .unwrap_or_default();
        match key.code {
            KeyCode::Up => self.input_cursor = self.input_cursor.saturating_sub(1),
            KeyCode::Down => {
                self.input_cursor = (self.input_cursor + 1).min(devices.len().saturating_sub(1));
            }
            KeyCode::Enter => {
                if let Some(device) = devices.get(self.input_cursor) {
                    return DisplayAction::JoypadSelect(device.id.clone());
                }
            }
            KeyCode::Char('e') => return DisplayAction::JoypadSetEnabled(true),
            KeyCode::Char('x') => return DisplayAction::JoypadSetEnabled(false),
            KeyCode::Char('r') => return DisplayAction::JoypadRescan,
            _ => {}
        }
        DisplayAction::None
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use crossterm::event::{KeyEventKind, KeyEventState};

    use super::*;
    use crate::participant_kind::ParticipantKind;
    use crate::stores::log_store::LogStore;
    use crate::stores::runtime_store::RuntimeStore;
    use crate::stores::telemetry_store::Timestamped;
    use crate::supervisor::{BoardSnapshot, ParticipantState, ParticipantStatus};
    use crate::telemetry::{
        JoypadDevice, JoypadDeviceStatus, JoypadDevicesSample, TelemetrySnapshot,
    };

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    fn with_model<T>(
        telemetry: &TelemetrySnapshot,
        run: impl FnOnce(&SessionViewModel<'_>) -> T,
    ) -> T {
        let board = BoardSnapshot::default();
        let logs = LogStore::new();
        let runtime = RuntimeStore::new();
        let model = SessionViewModel::new(&board, &logs, &runtime, telemetry, Instant::now());
        run(&model)
    }

    #[test]
    fn fixed_pages_cycle_without_mutating_their_labels() {
        let mut state = AppState::default();
        with_model(&TelemetrySnapshot::default(), |model| {
            for expected in [Page::Runtimes, Page::Logs, Page::Bus, Page::Input] {
                state.handle_key(key(KeyCode::Right), model);
                assert_eq!(state.page, expected);
            }
        });
        assert_eq!(
            Page::ALL.map(Page::label),
            ["Overview", "Runtimes", "Logs", "Bus", "Input"]
        );
    }

    #[test]
    fn first_input_attachment_requests_rescan() {
        let mut state = AppState::default();
        with_model(&TelemetrySnapshot::default(), |model| {
            assert_eq!(
                state.handle_key(key(KeyCode::Char('5')), model),
                DisplayAction::JoypadRescan
            );
        });
    }

    #[test]
    fn selection_and_enable_are_separate_authoritative_actions() {
        let now = Instant::now();
        let telemetry = TelemetrySnapshot {
            joypad: Some(Timestamped {
                received_at: now,
                value: JoypadDevicesSample {
                    available: vec![JoypadDevice {
                        id: "pad".to_string(),
                        name: "Pad".to_string(),
                        status: JoypadDeviceStatus::Ready,
                    }],
                    selected: None,
                    enabled: false,
                    unavailable_reason: None,
                    last_error: None,
                },
            }),
            ..TelemetrySnapshot::default()
        };
        let mut state = AppState {
            page: Page::Input,
            ..AppState::default()
        };
        with_model(&telemetry, |model| {
            assert_eq!(
                state.handle_key(key(KeyCode::Enter), model),
                DisplayAction::JoypadSelect("pad".to_string())
            );
            assert_eq!(
                state.handle_key(key(KeyCode::Char('e')), model),
                DisplayAction::JoypadSetEnabled(true)
            );
        });
        assert!(!telemetry.joypad.unwrap().value.enabled);
    }

    #[test]
    fn logs_keep_independent_filters_severity_and_follow_state() {
        let mut state = AppState {
            page: Page::Logs,
            ..AppState::default()
        };
        with_model(&TelemetrySnapshot::default(), |model| {
            state.handle_key(key(KeyCode::Char('s')), model);
            assert_eq!(state.log_severity, SeverityFilter::Error);
            state.handle_key(key(KeyCode::Char(' ')), model);
            assert!(!state.log_follow);
            state.handle_key(key(KeyCode::Char('/')), model);
            for character in ['f', 'a', 'i', 'l'] {
                state.handle_key(key(KeyCode::Char(character)), model);
            }
            state.handle_key(key(KeyCode::Enter), model);
        });
        assert_eq!(state.log_text_filter, "fail");
        assert!(state.log_runtime_filter.is_empty());
    }

    #[test]
    fn runtime_log_shortcut_uses_the_global_log_store_filter() {
        let mut board = BoardSnapshot::default();
        board.participants.insert(
            "motion".to_string(),
            ParticipantStatus::new("motion", ParticipantKind::Service, ParticipantState::Ready),
        );
        let logs = LogStore::new();
        let runtime = RuntimeStore::new();
        let telemetry = TelemetrySnapshot::default();
        let model = SessionViewModel::new(&board, &logs, &runtime, &telemetry, Instant::now());
        let mut state = AppState {
            page: Page::Runtimes,
            ..AppState::default()
        };
        state.handle_key(key(KeyCode::Char('l')), &model);
        assert_eq!(state.page, Page::Logs);
        assert_eq!(state.log_runtime_filter, "motion");
    }

    #[test]
    fn bus_sort_filter_and_internal_toggle_are_page_local() {
        let mut state = AppState {
            page: Page::Bus,
            ..AppState::default()
        };
        with_model(&TelemetrySnapshot::default(), |model| {
            state.handle_key(key(KeyCode::Char('s')), model);
            assert_eq!(state.bus_sort, BusSort::Topic);
            state.handle_key(key(KeyCode::Char('a')), model);
            assert!(state.bus_show_internal);
            state.handle_key(key(KeyCode::Char('/')), model);
            for character in ['r', 'o', 'u', 't', 'e', 'r'] {
                state.handle_key(key(KeyCode::Char(character)), model);
            }
            state.handle_key(key(KeyCode::Enter), model);
        });
        assert_eq!(state.bus_filter, "router");
        assert!(state.log_text_filter.is_empty());
    }
}
