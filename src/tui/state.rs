//! Pure five-page session navigation and input handling.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::display::DisplayAction;
use crate::session::controller::SessionMode;
use crate::stores::log_store::DisplayedLine;
use crate::stores::telemetry_store::DEFAULT_FRESHNESS_TTL;
use crate::supervisor::{LogSeverity, ParticipantStatus};
use crate::telemetry::TopicMetric;
use crate::tui::view_model::SessionViewModel;
use crate::tui::visibility::is_internal_id;

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
pub enum NavigationLevel {
    #[default]
    Tabs,
    Page,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LogSourceFilter {
    #[default]
    All,
    Runtimes,
    Tools,
}

impl LogSourceFilter {
    fn cycle(self) -> Self {
        match self {
            Self::All => Self::Runtimes,
            Self::Runtimes => Self::Tools,
            Self::Tools => Self::All,
        }
    }

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::All => "All",
            Self::Runtimes => "Runtimes",
            Self::Tools => "Tools",
        }
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
    LogParticipant,
    Bus,
}

#[derive(Debug, Clone)]
pub struct AppState {
    pub page: Page,
    pub(super) tab_cursor: usize,
    pub navigation: NavigationLevel,
    pub runtime_cursor: usize,
    runtime_cursor_id: Option<String>,
    pub runtime_detail_id: Option<String>,
    pub simulation: bool,
    pub log_source_filter: LogSourceFilter,
    pub log_filter_cursor: usize,
    pub log_text_filter: String,
    pub log_runtime_filter: String,
    pub log_severity: SeverityFilter,
    pub log_scroll: usize,
    pub log_follow: bool,
    pub log_pause_anchor: Option<std::time::Instant>,
    pub bus_filter: String,
    pub bus_sort: BusSort,
    pub bus_scroll: usize,
    pub bus_show_internal: bool,
    pub bus_control_cursor: usize,
    pub input_cursor: usize,
    input_cursor_id: Option<String>,
    pub show_help: bool,
    pub show_info: bool,
    editing: Option<Editing>,
    error_log_opened: bool,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            page: Page::Overview,
            tab_cursor: Page::Overview.index(),
            navigation: NavigationLevel::Tabs,
            runtime_cursor: 0,
            runtime_cursor_id: None,
            runtime_detail_id: None,
            simulation: false,
            log_source_filter: LogSourceFilter::All,
            log_filter_cursor: 0,
            log_text_filter: String::new(),
            log_runtime_filter: String::new(),
            log_severity: SeverityFilter::All,
            log_scroll: 0,
            log_follow: true,
            log_pause_anchor: None,
            bus_filter: String::new(),
            bus_sort: BusSort::Rate,
            bus_scroll: 0,
            bus_show_internal: false,
            bus_control_cursor: 0,
            input_cursor: 0,
            input_cursor_id: None,
            show_help: false,
            show_info: false,
            editing: None,
            error_log_opened: false,
        }
    }
}

impl AppState {
    #[must_use]
    pub fn for_mode(mode: SessionMode) -> Self {
        Self {
            simulation: mode == SessionMode::Simulation,
            ..Self::default()
        }
    }

    pub fn sync(&mut self, model: &SessionViewModel<'_>) {
        if let Some(detail_id) = self.runtime_detail_id.as_deref() {
            let detail_index = model
                .runtimes
                .iter()
                .position(|status| status.id == detail_id)
                .filter(|index| self.runtime_is_visible(model, *index));
            if let Some(index) = detail_index {
                self.runtime_cursor = index;
                self.runtime_cursor_id = Some(detail_id.to_string());
            } else {
                self.runtime_detail_id = None;
            }
        }
        let mut missing_selected_runtime = false;
        if let Some(cursor_id) = self.runtime_cursor_id.as_deref() {
            if let Some(index) = model
                .runtimes
                .iter()
                .position(|status| status.id == cursor_id)
                .filter(|index| self.runtime_is_visible(model, *index))
            {
                self.runtime_cursor = index;
            } else {
                self.runtime_cursor = model.runtimes.len();
                missing_selected_runtime = true;
            }
        }
        let visible = self.visible_runtime_indices(model);
        if !missing_selected_runtime && !visible.contains(&self.runtime_cursor) {
            self.runtime_detail_id = None;
            self.runtime_cursor = visible.first().copied().unwrap_or(0);
        }
        if !missing_selected_runtime {
            self.runtime_cursor_id = model
                .runtimes
                .get(self.runtime_cursor)
                .filter(|_| visible.contains(&self.runtime_cursor))
                .map(|status| status.id.clone());
        }

        let devices = model
            .telemetry
            .joypad
            .as_ref()
            .filter(|joypad| !joypad.is_stale(model.now, DEFAULT_FRESHNESS_TTL))
            .map(|joypad| joypad.value.available.as_slice())
            .unwrap_or_default();
        let mut missing_selected_device = false;
        if let Some(cursor_id) = self.input_cursor_id.as_deref() {
            if let Some(index) = devices.iter().position(|device| device.id == cursor_id) {
                self.input_cursor = index;
            } else {
                self.input_cursor = devices.len();
                missing_selected_device = true;
            }
        }
        if !missing_selected_device {
            self.input_cursor = self.input_cursor.min(devices.len().saturating_sub(1));
            self.input_cursor_id = devices
                .get(self.input_cursor)
                .map(|device| device.id.clone());
        }
        if self.log_follow {
            self.log_scroll = 0;
        }
    }

    #[must_use]
    pub fn editing_label(&self) -> Option<&'static str> {
        self.editing.map(|editing| match editing {
            Editing::LogText => "contains",
            Editing::LogParticipant => "participant",
            Editing::Bus => "bus",
        })
    }

    pub fn open_logs_for_error(&mut self) {
        if self.error_log_opened {
            return;
        }
        self.error_log_opened = true;
        self.editing = None;
        self.runtime_detail_id = None;
        self.show_help = false;
        self.show_info = false;
        self.page = Page::Logs;
        self.tab_cursor = Page::Logs.index();
        self.navigation = NavigationLevel::Page;
        self.log_source_filter = LogSourceFilter::All;
        self.log_runtime_filter.clear();
        self.log_text_filter.clear();
        self.log_severity = SeverityFilter::Error;
        self.log_scroll = 0;
        self.log_follow = true;
        self.log_pause_anchor = None;
    }

    pub fn handle_key(&mut self, key: KeyEvent, model: &SessionViewModel<'_>) -> DisplayAction {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return DisplayAction::Quit;
        }
        if let Some(editing) = self.editing {
            return self.handle_editing(key, editing);
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
                return self.activate_page(Page::Overview);
            }
            KeyCode::Char('2') if !self.show_help && !self.show_info => {
                return self.activate_page(Page::Runtimes);
            }
            KeyCode::Char('3') if !self.show_help && !self.show_info => {
                return self.activate_page(Page::Logs);
            }
            KeyCode::Char('4') if !self.show_help && !self.show_info => {
                return self.activate_page(Page::Bus);
            }
            KeyCode::Char('5') if !self.show_help && !self.show_info => {
                return self.activate_page(Page::Input);
            }
            _ => {}
        }
        if self.show_help || self.show_info {
            return DisplayAction::None;
        }

        if key.code == KeyCode::Esc {
            if self.page == Page::Runtimes && self.runtime_detail_id.take().is_some() {
                return DisplayAction::None;
            }
            if self.navigation == NavigationLevel::Page {
                self.navigation = NavigationLevel::Tabs;
                self.tab_cursor = self.page.index();
            } else {
                self.tab_cursor = self.page.index();
            }
            return DisplayAction::None;
        }

        if self.navigation == NavigationLevel::Tabs {
            match key.code {
                KeyCode::Left | KeyCode::Up => {
                    let page = self.tab_page().offset(-1);
                    self.tab_cursor = page.index();
                }
                KeyCode::Right | KeyCode::Down => {
                    let page = self.tab_page().offset(1);
                    self.tab_cursor = page.index();
                }
                KeyCode::Enter => return self.activate_page(self.tab_page()),
                _ => {}
            }
            return DisplayAction::None;
        }

        match self.page {
            Page::Overview => DisplayAction::None,
            Page::Runtimes => self.handle_runtimes(key, model),
            Page::Logs => self.handle_logs(key, model),
            Page::Bus => self.handle_bus(key),
            Page::Input => self.handle_input_page(key, model),
        }
    }

    fn activate_page(&mut self, page: Page) -> DisplayAction {
        let changed = self.page != page;
        self.page = page;
        self.tab_cursor = page.index();
        self.navigation = NavigationLevel::Page;
        self.runtime_detail_id = None;
        if changed && page == Page::Input {
            DisplayAction::JoypadRescan
        } else {
            DisplayAction::None
        }
    }

    fn tab_page(&self) -> Page {
        Page::ALL.get(self.tab_cursor).copied().unwrap_or(self.page)
    }

    fn handle_editing(&mut self, key: KeyEvent, editing: Editing) -> DisplayAction {
        match key.code {
            KeyCode::Esc | KeyCode::Enter => self.editing = None,
            KeyCode::Backspace => {
                self.active_filter_mut(editing).pop();
            }
            KeyCode::Char(character)
                if !key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT) =>
            {
                self.active_filter_mut(editing).push(character);
            }
            _ => {}
        }
        DisplayAction::None
    }

    fn active_filter_mut(&mut self, editing: Editing) -> &mut String {
        match editing {
            Editing::LogText => &mut self.log_text_filter,
            Editing::LogParticipant => &mut self.log_runtime_filter,
            Editing::Bus => &mut self.bus_filter,
        }
    }

    fn handle_runtimes(&mut self, key: KeyEvent, model: &SessionViewModel<'_>) -> DisplayAction {
        match key.code {
            KeyCode::Enter => {
                self.runtime_detail_id =
                    self.selected_runtime(model).map(|status| status.id.clone());
                self.runtime_cursor_id.clone_from(&self.runtime_detail_id);
            }
            KeyCode::Up if self.runtime_detail_id.is_none() => {
                self.move_runtime_cursor(model, -1);
            }
            KeyCode::Down => {
                if self.runtime_detail_id.is_none() {
                    self.move_runtime_cursor(model, 1);
                }
            }
            KeyCode::Char('r') => {
                if let Some(status) = self.selected_runtime(model) {
                    return DisplayAction::Restart(status.id.clone());
                }
            }
            KeyCode::Char('l') => {
                if let Some(status) = self.selected_runtime(model) {
                    self.log_runtime_filter.clone_from(&status.id);
                    self.log_source_filter = LogSourceFilter::Runtimes;
                    self.log_text_filter.clear();
                    self.log_severity = SeverityFilter::All;
                    self.log_scroll = 0;
                    self.log_follow = true;
                    self.log_pause_anchor = None;
                    self.page = Page::Logs;
                    self.tab_cursor = Page::Logs.index();
                    self.runtime_detail_id = None;
                }
            }
            _ => {}
        }
        DisplayAction::None
    }

    fn runtime_is_visible(&self, model: &SessionViewModel<'_>, index: usize) -> bool {
        model
            .runtimes
            .get(index)
            .is_some_and(|status| model.runtime_is_loaded(status, self.simulation))
    }

    fn visible_runtime_indices(&self, model: &SessionViewModel<'_>) -> Vec<usize> {
        model
            .runtimes
            .iter()
            .enumerate()
            .filter_map(|(index, _)| self.runtime_is_visible(model, index).then_some(index))
            .collect()
    }

    fn selected_runtime<'a>(
        &self,
        model: &'a SessionViewModel<'_>,
    ) -> Option<&'a ParticipantStatus> {
        if let Some(cursor_id) = self.runtime_cursor_id.as_deref() {
            return model
                .runtimes
                .iter()
                .copied()
                .find(|status| status.id == cursor_id)
                .filter(|status| model.runtime_is_loaded(status, self.simulation));
        }
        self.runtime_is_visible(model, self.runtime_cursor)
            .then(|| model.runtimes[self.runtime_cursor])
    }

    fn move_runtime_cursor(&mut self, model: &SessionViewModel<'_>, offset: isize) {
        let visible = self.visible_runtime_indices(model);
        let Some(position) = visible
            .iter()
            .position(|index| *index == self.runtime_cursor)
        else {
            self.runtime_cursor = visible.first().copied().unwrap_or(0);
            self.runtime_cursor_id = model
                .runtimes
                .get(self.runtime_cursor)
                .filter(|_| visible.contains(&self.runtime_cursor))
                .map(|status| status.id.clone());
            return;
        };
        let target = position
            .saturating_add_signed(offset)
            .min(visible.len().saturating_sub(1));
        self.runtime_cursor = visible[target];
        self.runtime_cursor_id = model
            .runtimes
            .get(self.runtime_cursor)
            .map(|status| status.id.clone());
    }

    fn handle_logs(&mut self, key: KeyEvent, model: &SessionViewModel<'_>) -> DisplayAction {
        match key.code {
            KeyCode::Char('/') => {
                self.log_filter_cursor = 3;
                self.editing = Some(Editing::LogText);
            }
            KeyCode::Char('f') => {
                self.log_filter_cursor = 1;
                self.editing = Some(Editing::LogParticipant);
            }
            KeyCode::Char('s') => {
                self.log_filter_cursor = 2;
                self.log_severity = self.log_severity.cycle();
            }
            KeyCode::Left => {
                self.log_filter_cursor = self.log_filter_cursor.checked_sub(1).unwrap_or(4);
            }
            KeyCode::Right => self.log_filter_cursor = (self.log_filter_cursor + 1) % 5,
            KeyCode::Enter => match self.log_filter_cursor {
                0 => self.log_source_filter = self.log_source_filter.cycle(),
                1 => self.editing = Some(Editing::LogParticipant),
                2 => self.log_severity = self.log_severity.cycle(),
                3 => self.editing = Some(Editing::LogText),
                4 => self.toggle_log_follow(model),
                _ => {}
            },
            KeyCode::Char(' ') => {
                self.log_filter_cursor = 4;
                self.toggle_log_follow(model);
            }
            KeyCode::Up => {
                self.pause_logs(model);
                self.log_scroll = self.log_scroll.saturating_add(1);
            }
            KeyCode::Down => self.log_scroll = self.log_scroll.saturating_sub(1),
            KeyCode::End => {
                self.log_scroll = 0;
                self.log_follow = true;
                self.log_pause_anchor = None;
            }
            _ => {}
        }
        DisplayAction::None
    }

    fn toggle_log_follow(&mut self, model: &SessionViewModel<'_>) {
        if self.log_follow {
            self.pause_logs(model);
        } else {
            self.log_follow = true;
            self.log_scroll = 0;
            self.log_pause_anchor = None;
        }
    }

    fn pause_logs(&mut self, model: &SessionViewModel<'_>) {
        if self.log_follow {
            self.log_pause_anchor = Some(model.now);
        }
        self.log_follow = false;
    }

    fn handle_bus(&mut self, key: KeyEvent) -> DisplayAction {
        match key.code {
            KeyCode::Char('/') => {
                self.bus_control_cursor = 0;
                self.editing = Some(Editing::Bus);
            }
            KeyCode::Char('s') => {
                self.bus_control_cursor = 1;
                self.bus_sort = self.bus_sort.cycle();
            }
            KeyCode::Char('a') => {
                self.bus_control_cursor = 2;
                self.bus_show_internal = !self.bus_show_internal;
            }
            KeyCode::Left => {
                self.bus_control_cursor = self.bus_control_cursor.checked_sub(1).unwrap_or(2);
            }
            KeyCode::Right => self.bus_control_cursor = (self.bus_control_cursor + 1) % 3,
            KeyCode::Enter => match self.bus_control_cursor {
                0 => self.editing = Some(Editing::Bus),
                1 => self.bus_sort = self.bus_sort.cycle(),
                2 => self.bus_show_internal = !self.bus_show_internal,
                _ => {}
            },
            KeyCode::Up => self.bus_scroll = self.bus_scroll.saturating_sub(1),
            KeyCode::Down => self.bus_scroll = self.bus_scroll.saturating_add(1),
            _ => {}
        }
        DisplayAction::None
    }

    fn handle_input_page(&mut self, key: KeyEvent, model: &SessionViewModel<'_>) -> DisplayAction {
        let devices = model
            .telemetry
            .joypad
            .as_ref()
            .filter(|joypad| !joypad.is_stale(model.now, DEFAULT_FRESHNESS_TTL))
            .map(|joypad| joypad.value.available.as_slice())
            .unwrap_or_default();
        match key.code {
            KeyCode::Up => {
                self.input_cursor = if self.input_cursor >= devices.len() {
                    devices.len().saturating_sub(1)
                } else {
                    self.input_cursor.saturating_sub(1)
                };
                self.input_cursor_id = devices
                    .get(self.input_cursor)
                    .map(|device| device.id.clone());
            }
            KeyCode::Down => {
                self.input_cursor = if self.input_cursor >= devices.len() {
                    0
                } else {
                    (self.input_cursor + 1).min(devices.len().saturating_sub(1))
                };
                self.input_cursor_id = devices
                    .get(self.input_cursor)
                    .map(|device| device.id.clone());
            }
            KeyCode::Enter => {
                let selected = self.input_cursor_id.as_deref().map_or_else(
                    || devices.get(self.input_cursor),
                    |id| devices.iter().find(|device| device.id == id),
                );
                if let Some(device) = selected {
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

    #[cfg(test)]
    fn filtered_log_count(&self, model: &SessionViewModel<'_>) -> usize {
        let participant = CaseInsensitiveNeedle::new(&self.log_runtime_filter);
        let text = CaseInsensitiveNeedle::new(&self.log_text_filter);
        model
            .logs
            .lines()
            .filter(|line| self.log_line_matches(line, model, &participant, &text))
            .filter(|line| {
                self.log_follow
                    || self
                        .log_pause_anchor
                        .is_none_or(|anchor| line.received_at <= anchor)
            })
            .count()
    }

    pub(super) fn log_line_matches(
        &self,
        line: &DisplayedLine,
        model: &SessionViewModel<'_>,
        participant: &CaseInsensitiveNeedle<'_>,
        text: &CaseInsensitiveNeedle<'_>,
    ) -> bool {
        let source_matches = match self.log_source_filter {
            LogSourceFilter::All => true,
            LogSourceFilter::Runtimes => {
                !is_internal_id(&line.participant, model.board, model.runtime)
            }
            LogSourceFilter::Tools => is_internal_id(&line.participant, model.board, model.runtime),
        };
        source_matches
            && participant.contains(&line.participant)
            && text.contains(&line.text)
            && self.log_severity.matches(line.severity)
    }

    #[cfg(test)]
    fn filtered_bus_count(&self, model: &SessionViewModel<'_>) -> usize {
        let filter = CaseInsensitiveNeedle::new(&self.bus_filter);
        model.telemetry.router.as_ref().map_or(0, |sample| {
            sample
                .value
                .topics
                .iter()
                .filter(|metric| self.bus_metric_matches(metric, model, &filter))
                .count()
        })
    }

    pub(super) fn bus_metric_matches(
        &self,
        metric: &TopicMetric,
        model: &SessionViewModel<'_>,
        filter: &CaseInsensitiveNeedle<'_>,
    ) -> bool {
        let visibility_matches = metric.aggregate_overflow
            || self.bus_show_internal
            || !is_internal_id(&metric.from_participant, model.board, model.runtime);
        if !visibility_matches {
            return false;
        }
        filter.is_empty()
            || filter.contains(&metric.topic)
            || filter.contains(&metric.from_participant)
    }
}

pub(super) struct CaseInsensitiveNeedle<'a> {
    raw: &'a str,
    folded: Option<Vec<char>>,
}

impl<'a> CaseInsensitiveNeedle<'a> {
    pub(super) fn new(raw: &'a str) -> Self {
        let folded = (!raw.is_ascii()).then(|| raw.chars().flat_map(char::to_lowercase).collect());
        Self { raw, folded }
    }

    fn is_empty(&self) -> bool {
        self.raw.is_empty()
    }

    fn contains(&self, haystack: &str) -> bool {
        if self.raw.is_empty() {
            return true;
        }
        if self.raw.is_ascii() && haystack.is_ascii() {
            return haystack
                .as_bytes()
                .windows(self.raw.len())
                .any(|window| window.eq_ignore_ascii_case(self.raw.as_bytes()));
        }
        haystack.char_indices().any(|(start, _)| {
            let mut actual = haystack[start..].chars().flat_map(char::to_lowercase);
            match &self.folded {
                Some(expected) => expected
                    .iter()
                    .all(|expected| actual.next() == Some(*expected)),
                None => self.raw.bytes().all(|expected| {
                    actual.next() == Some(char::from(expected.to_ascii_lowercase()))
                }),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use crossterm::event::{KeyEventKind, KeyEventState};

    use super::*;
    use crate::stores::log_store::LogStore;
    use crate::stores::runtime_store::RuntimeStore;
    use phoxal_cli_core::session::ParticipantKind;

    #[test]
    fn non_ascii_filters_match_case_insensitively() {
        assert!(CaseInsensitiveNeedle::new("är").contains("Ärger"));
        assert!(CaseInsensitiveNeedle::new("café").contains("CAFÉ"));
        assert!(CaseInsensitiveNeedle::new("σ").contains("ΟΔΟΣ"));
    }
    use crate::stores::telemetry_store::Timestamped;
    use crate::supervisor::{
        BoardSnapshot, LogSource, ParticipantState, ParticipantStatus, RoutedLogLine,
    };
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
    fn tabs_preview_with_arrows_and_activate_with_enter() {
        let mut state = AppState::default();
        with_model(&TelemetrySnapshot::default(), |model| {
            for expected in [Page::Runtimes, Page::Logs, Page::Bus, Page::Input] {
                state.handle_key(key(KeyCode::Right), model);
                assert_eq!(state.tab_cursor, expected.index());
                state.handle_key(key(KeyCode::Enter), model);
                assert_eq!(state.page, expected);
                assert_eq!(state.navigation, NavigationLevel::Page);
                state.handle_key(key(KeyCode::Esc), model);
                assert_eq!(state.navigation, NavigationLevel::Tabs);
                assert_eq!(state.tab_cursor, expected.index());
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
                    }]
                    .into(),
                    devices_truncated: 0,
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
            navigation: NavigationLevel::Page,
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
    fn disappearing_input_device_does_not_retarget_selection() {
        let now = Instant::now();
        let devices = |ids: &[&str]| TelemetrySnapshot {
            joypad: Some(Timestamped {
                received_at: now,
                value: JoypadDevicesSample {
                    available: ids
                        .iter()
                        .map(|id| JoypadDevice {
                            id: (*id).to_string(),
                            name: (*id).to_string(),
                            status: JoypadDeviceStatus::Ready,
                        })
                        .collect::<Vec<_>>()
                        .into(),
                    ..JoypadDevicesSample::default()
                },
            }),
            ..TelemetrySnapshot::default()
        };
        let mut state = AppState {
            page: Page::Input,
            navigation: NavigationLevel::Page,
            ..AppState::default()
        };
        let both = devices(&["pad-a", "pad-b"]);
        with_model(&both, |model| {
            state.sync(model);
            state.handle_key(key(KeyCode::Down), model);
            assert_eq!(
                state.handle_key(key(KeyCode::Enter), model),
                DisplayAction::JoypadSelect("pad-b".to_string())
            );
        });

        let only_a = devices(&["pad-a"]);
        with_model(&only_a, |model| {
            state.sync(model);
            assert_eq!(state.input_cursor, 1);
            assert_eq!(
                state.handle_key(key(KeyCode::Enter), model),
                DisplayAction::None
            );
            state.handle_key(key(KeyCode::Up), model);
            assert_eq!(
                state.handle_key(key(KeyCode::Enter), model),
                DisplayAction::JoypadSelect("pad-a".to_string())
            );
        });
    }

    #[test]
    fn stale_input_device_cannot_be_selected() {
        let telemetry = TelemetrySnapshot {
            joypad: Some(Timestamped {
                received_at: Instant::now()
                    - DEFAULT_FRESHNESS_TTL
                    - std::time::Duration::from_secs(1),
                value: JoypadDevicesSample {
                    available: vec![JoypadDevice {
                        id: "stale-pad".to_string(),
                        name: "Stale Pad".to_string(),
                        status: JoypadDeviceStatus::Ready,
                    }]
                    .into(),
                    ..JoypadDevicesSample::default()
                },
            }),
            ..TelemetrySnapshot::default()
        };
        let mut state = AppState {
            page: Page::Input,
            navigation: NavigationLevel::Page,
            ..AppState::default()
        };
        with_model(&telemetry, |model| {
            state.sync(model);
            assert!(state.input_cursor_id.is_none());
            assert_eq!(
                state.handle_key(key(KeyCode::Enter), model),
                DisplayAction::None
            );
        });
    }

    #[test]
    fn logs_keep_independent_filters_severity_and_follow_state() {
        let mut state = AppState {
            page: Page::Logs,
            navigation: NavigationLevel::Page,
            ..AppState::default()
        };
        with_model(&TelemetrySnapshot::default(), |model| {
            state.handle_key(key(KeyCode::Char('s')), model);
            assert_eq!(state.log_severity, SeverityFilter::Error);
            assert_eq!(state.log_filter_cursor, 2);
            state.handle_key(key(KeyCode::Char(' ')), model);
            assert!(!state.log_follow);
            assert_eq!(state.log_filter_cursor, 4);
            state.handle_key(key(KeyCode::Char('/')), model);
            assert_eq!(state.log_filter_cursor, 3);
            for character in ['f', 'a', 'i', 'l'] {
                state.handle_key(key(KeyCode::Char(character)), model);
            }
            state.handle_key(key(KeyCode::Enter), model);
            state.handle_key(key(KeyCode::Char('f')), model);
            assert_eq!(state.log_filter_cursor, 1);
            state.handle_key(key(KeyCode::Esc), model);
        });
        assert_eq!(state.log_text_filter, "fail");
        assert!(state.log_runtime_filter.is_empty());
    }

    #[test]
    fn paused_logs_exclude_lines_received_after_the_pause_anchor() {
        let started = Instant::now();
        let mut logs = LogStore::new();
        logs.record_at(
            RoutedLogLine {
                participant: "motion".to_string(),
                source: LogSource::Bus,
                severity: LogSeverity::Info,
                text: "before pause".to_string(),
            },
            started,
        );
        let board = BoardSnapshot::default();
        let runtime = RuntimeStore::new();
        let telemetry = TelemetrySnapshot::default();
        let mut state = AppState {
            page: Page::Logs,
            navigation: NavigationLevel::Page,
            ..AppState::default()
        };
        {
            let model = SessionViewModel::new(&board, &logs, &runtime, &telemetry, started);
            state.handle_key(key(KeyCode::Char(' ')), &model);
            assert_eq!(state.log_pause_anchor, Some(started));
            assert_eq!(state.filtered_log_count(&model), 1);
        }

        logs.record_at(
            RoutedLogLine {
                participant: "motion".to_string(),
                source: LogSource::Bus,
                severity: LogSeverity::Info,
                text: "after pause".to_string(),
            },
            started + std::time::Duration::from_secs(1),
        );
        let model = SessionViewModel::new(&board, &logs, &runtime, &telemetry, started);
        assert_eq!(state.filtered_log_count(&model), 1);
        assert!(!state.log_follow);
    }

    #[test]
    fn pausing_an_empty_log_view_freezes_before_the_first_matching_line() {
        let board = BoardSnapshot::default();
        let mut logs = LogStore::new();
        let runtime = RuntimeStore::new();
        let telemetry = TelemetrySnapshot::default();
        let mut state = AppState {
            page: Page::Logs,
            navigation: NavigationLevel::Page,
            ..AppState::default()
        };
        {
            let model = SessionViewModel::new(&board, &logs, &runtime, &telemetry, Instant::now());
            state.handle_key(key(KeyCode::Char(' ')), &model);
        }
        let anchor = state
            .log_pause_anchor
            .expect("paused empty view still needs a wall-time anchor");
        logs.record_at(
            RoutedLogLine {
                participant: "motion".to_string(),
                source: LogSource::Bus,
                severity: LogSeverity::Info,
                text: "first line after pause".to_string(),
            },
            anchor + std::time::Duration::from_secs(1),
        );
        let model = SessionViewModel::new(&board, &logs, &runtime, &telemetry, anchor);
        assert_eq!(state.filtered_log_count(&model), 0);
        assert!(!state.log_follow);
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
            navigation: NavigationLevel::Page,
            log_source_filter: LogSourceFilter::Tools,
            log_text_filter: "old".to_string(),
            log_severity: SeverityFilter::Warn,
            ..AppState::default()
        };
        state.handle_key(key(KeyCode::Char('l')), &model);
        assert_eq!(state.page, Page::Logs);
        assert_eq!(state.log_runtime_filter, "motion");
        assert_eq!(state.log_source_filter, LogSourceFilter::Runtimes);
        assert!(state.log_text_filter.is_empty());
        assert_eq!(state.log_severity, SeverityFilter::All);
    }

    #[test]
    fn bus_sort_filter_and_internal_toggle_are_page_local() {
        let mut state = AppState {
            page: Page::Bus,
            navigation: NavigationLevel::Page,
            ..AppState::default()
        };
        with_model(&TelemetrySnapshot::default(), |model| {
            state.handle_key(key(KeyCode::Char('s')), model);
            assert_eq!(state.bus_sort, BusSort::Topic);
            assert_eq!(state.bus_control_cursor, 1);
            state.handle_key(key(KeyCode::Char('a')), model);
            assert!(state.bus_show_internal);
            assert_eq!(state.bus_control_cursor, 2);
            state.handle_key(key(KeyCode::Char('/')), model);
            assert_eq!(state.bus_control_cursor, 0);
            for character in ['r', 'o', 'u', 't', 'e', 'r'] {
                state.handle_key(key(KeyCode::Char(character)), model);
            }
            state.handle_key(key(KeyCode::Enter), model);
        });
        assert_eq!(state.bus_filter, "router");
        assert!(state.log_text_filter.is_empty());
    }

    #[test]
    fn bus_filter_does_not_bypass_internal_visibility() {
        let telemetry = TelemetrySnapshot {
            router: Some(Timestamped {
                received_at: Instant::now(),
                value: crate::telemetry::RouterMetricsSample {
                    topics: vec![crate::telemetry::TopicMetric {
                        topic: "dev/robots/rover/v2/router/metrics".to_string(),
                        from_participant: "tool-router".to_string(),
                        ingress_rate_hz: 1.0,
                        count: 1,
                        aggregate_overflow: false,
                    }]
                    .into(),
                    ..crate::telemetry::RouterMetricsSample::default()
                },
            }),
            ..TelemetrySnapshot::default()
        };
        let mut state = AppState {
            bus_filter: "router".to_string(),
            ..AppState::default()
        };
        with_model(&telemetry, |model| {
            assert_eq!(state.filtered_bus_count(model), 0);
            state.bus_show_internal = true;
            assert_eq!(state.filtered_bus_count(model), 1);
        });
    }

    #[test]
    fn bus_overflow_stays_visible_and_filters_match_sanitized_cells() {
        let telemetry = TelemetrySnapshot {
            router: Some(Timestamped {
                received_at: Instant::now(),
                value: crate::telemetry::RouterMetricsSample {
                    topics: vec![
                        crate::telemetry::TopicMetric {
                            topic: "drive/state".to_string(),
                            from_participant: "motion".to_string(),
                            ingress_rate_hz: 1.0,
                            count: 1,
                            aggregate_overflow: false,
                        },
                        crate::telemetry::TopicMetric {
                            topic: "Other/unobserved traffic".to_string(),
                            from_participant: "multiple".to_string(),
                            ingress_rate_hz: 2.0,
                            count: 2,
                            aggregate_overflow: true,
                        },
                    ]
                    .into(),
                    ..crate::telemetry::RouterMetricsSample::default()
                },
            }),
            ..TelemetrySnapshot::default()
        };
        let mut state = AppState {
            bus_filter: "drive/state".to_string(),
            ..AppState::default()
        };
        with_model(&telemetry, |model| {
            assert_eq!(state.filtered_bus_count(model), 1);
            state.bus_filter.clear();
            assert_eq!(state.filtered_bus_count(model), 2);
            state.bus_show_internal = true;
            assert_eq!(state.filtered_bus_count(model), 2);
        });
    }

    #[test]
    fn bus_arrows_move_the_topic_window_in_reading_order() {
        let mut state = AppState {
            page: Page::Bus,
            navigation: NavigationLevel::Page,
            bus_scroll: 1,
            ..AppState::default()
        };
        with_model(&TelemetrySnapshot::default(), |model| {
            state.handle_key(key(KeyCode::Up), model);
            assert_eq!(state.bus_scroll, 0);
            state.handle_key(key(KeyCode::Down), model);
            assert_eq!(state.bus_scroll, 1);
        });
    }

    #[test]
    fn error_shortcut_exits_editing_and_reveals_the_new_error() {
        let mut state = AppState {
            page: Page::Bus,
            navigation: NavigationLevel::Page,
            runtime_detail_id: Some("motion".to_string()),
            log_severity: SeverityFilter::Warn,
            log_source_filter: LogSourceFilter::Tools,
            log_runtime_filter: "motion".to_string(),
            log_text_filter: "old".to_string(),
            ..AppState::default()
        };
        with_model(&TelemetrySnapshot::default(), |model| {
            state.handle_key(key(KeyCode::Char('/')), model);
            assert_eq!(state.editing, Some(Editing::Bus));
            state.show_help = true;
            state.open_logs_for_error();
            assert_eq!(state.page, Page::Logs);
            assert_eq!(state.editing, None);
            assert!(state.runtime_detail_id.is_none());
            assert!(!state.show_help);
            assert_eq!(state.log_severity, SeverityFilter::Error);
            assert_eq!(state.log_source_filter, LogSourceFilter::All);
            assert!(state.log_runtime_filter.is_empty());
            assert!(state.log_text_filter.is_empty());
            state.page = Page::Bus;
            state.handle_key(key(KeyCode::Char('/')), model);
            assert_eq!(state.editing, Some(Editing::Bus));
            state.open_logs_for_error();
            assert_eq!(state.page, Page::Bus);
            assert_eq!(state.editing, Some(Editing::Bus));
            state.handle_key(key(KeyCode::Char('x')), model);
        });
        assert_eq!(state.bus_filter, "x");

        let mut default_state = AppState::default();
        default_state.open_logs_for_error();
        assert_eq!(default_state.log_severity, SeverityFilter::Error);
    }

    #[test]
    fn sync_leaves_page_window_clamping_to_the_renderer() {
        let mut logs = LogStore::new();
        logs.record(crate::supervisor::RoutedLogLine {
            participant: "motion".to_string(),
            source: crate::supervisor::LogSource::Bus,
            severity: LogSeverity::Info,
            text: "ready".to_string(),
        });
        let board = BoardSnapshot::default();
        let runtime = RuntimeStore::new();
        let now = Instant::now();
        let telemetry = TelemetrySnapshot {
            router: Some(Timestamped {
                received_at: now,
                value: crate::telemetry::RouterMetricsSample {
                    topics: vec![crate::telemetry::TopicMetric {
                        topic: "v1/motion/state".to_string(),
                        from_participant: "motion".to_string(),
                        ingress_rate_hz: 1.0,
                        count: 1,
                        aggregate_overflow: false,
                    }]
                    .into(),
                    topics_truncated: 0,
                    throughput_msg_s: 1.0,
                    window_ns: 1,
                },
            }),
            ..TelemetrySnapshot::default()
        };
        let model = SessionViewModel::new(&board, &logs, &runtime, &telemetry, now);
        let mut state = AppState {
            log_follow: false,
            log_scroll: usize::MAX,
            bus_scroll: usize::MAX,
            ..AppState::default()
        };
        state.sync(&model);
        assert_eq!(state.log_scroll, usize::MAX);
        assert_eq!(state.bus_scroll, usize::MAX);
        state.log_follow = true;
        state.sync(&model);
        assert_eq!(state.log_scroll, 0);
    }

    #[test]
    fn invalid_tab_cursor_falls_back_to_the_active_page() {
        let mut state = AppState {
            page: Page::Logs,
            tab_cursor: usize::MAX,
            navigation: NavigationLevel::Tabs,
            ..AppState::default()
        };
        with_model(&TelemetrySnapshot::default(), |model| {
            state.handle_key(key(KeyCode::Right), model);
            assert_eq!(state.tab_cursor, Page::Bus.index());
            state.tab_cursor = usize::MAX;
            state.handle_key(key(KeyCode::Enter), model);
            assert_eq!(state.page, Page::Logs);
        });
    }

    #[test]
    fn runtime_detail_requires_escape_before_arrows_change_runtime() {
        let mut board = BoardSnapshot::default();
        for id in ["alpha", "beta"] {
            board.participants.insert(
                id.to_string(),
                ParticipantStatus::new(id, ParticipantKind::Service, ParticipantState::Ready),
            );
        }
        let logs = LogStore::new();
        let runtime = RuntimeStore::new();
        let telemetry = TelemetrySnapshot::default();
        let model = SessionViewModel::new(&board, &logs, &runtime, &telemetry, Instant::now());
        let mut state = AppState {
            page: Page::Runtimes,
            navigation: NavigationLevel::Page,
            ..AppState::default()
        };
        state.handle_key(key(KeyCode::Enter), &model);
        assert_eq!(state.runtime_detail_id.as_deref(), Some("alpha"));
        state.handle_key(key(KeyCode::Down), &model);
        assert_eq!(state.runtime_cursor, 0);
        assert_eq!(state.runtime_detail_id.as_deref(), Some("alpha"));
        state.handle_key(key(KeyCode::Esc), &model);
        state.handle_key(key(KeyCode::Down), &model);
        assert_eq!(state.runtime_cursor, 1);
    }

    #[test]
    fn runtime_cursor_tracks_identity_across_live_state_resorting() {
        let logs = LogStore::new();
        let runtime = RuntimeStore::new();
        let telemetry = TelemetrySnapshot::default();
        let mut initial_board = BoardSnapshot::default();
        for id in ["alpha", "beta"] {
            initial_board.participants.insert(
                id.to_string(),
                ParticipantStatus::new(id, ParticipantKind::Service, ParticipantState::Ready),
            );
        }
        let mut state = AppState {
            page: Page::Runtimes,
            navigation: NavigationLevel::Page,
            ..AppState::default()
        };
        let initial =
            SessionViewModel::new(&initial_board, &logs, &runtime, &telemetry, Instant::now());
        state.sync(&initial);
        state.handle_key(key(KeyCode::Down), &initial);
        assert_eq!(
            state.handle_key(key(KeyCode::Char('r')), &initial),
            DisplayAction::Restart("beta".to_string())
        );

        let mut resorted_board = initial_board;
        resorted_board
            .participants
            .get_mut("beta")
            .expect("beta")
            .state = ParticipantState::Failed;
        let resorted =
            SessionViewModel::new(&resorted_board, &logs, &runtime, &telemetry, Instant::now());
        state.sync(&resorted);
        assert_eq!(state.runtime_cursor, 0);
        assert_eq!(
            state.handle_key(key(KeyCode::Char('r')), &resorted),
            DisplayAction::Restart("beta".to_string())
        );
    }

    #[test]
    fn disappearing_runtime_does_not_retarget_restart_to_its_old_index() {
        let logs = LogStore::new();
        let runtime = RuntimeStore::new();
        let telemetry = TelemetrySnapshot::default();
        let mut board = BoardSnapshot::default();
        for id in ["alpha", "beta", "gamma"] {
            board.participants.insert(
                id.to_string(),
                ParticipantStatus::new(id, ParticipantKind::Service, ParticipantState::Ready),
            );
        }
        let mut state = AppState {
            page: Page::Runtimes,
            navigation: NavigationLevel::Page,
            ..AppState::default()
        };
        let initial = SessionViewModel::new(&board, &logs, &runtime, &telemetry, Instant::now());
        state.sync(&initial);
        state.handle_key(key(KeyCode::Down), &initial);
        assert_eq!(
            state.handle_key(key(KeyCode::Char('r')), &initial),
            DisplayAction::Restart("beta".to_string())
        );

        board.participants.remove("beta");
        let after_removal =
            SessionViewModel::new(&board, &logs, &runtime, &telemetry, Instant::now());
        state.sync(&after_removal);
        assert_eq!(
            state.handle_key(key(KeyCode::Char('r')), &after_removal),
            DisplayAction::None
        );

        state.handle_key(key(KeyCode::Down), &after_removal);
        assert_eq!(
            state.handle_key(key(KeyCode::Char('r')), &after_removal),
            DisplayAction::Restart("alpha".to_string())
        );
    }

    #[test]
    fn simulation_runtime_navigation_skips_physical_driver_rows() {
        let mut board = BoardSnapshot::default();
        board.participants.insert(
            "alpha".to_string(),
            ParticipantStatus::new("alpha", ParticipantKind::Service, ParticipantState::Ready),
        );
        board.participants.insert(
            "front_camera".to_string(),
            ParticipantStatus::new(
                "front_camera",
                ParticipantKind::Driver,
                ParticipantState::Ready,
            ),
        );
        let logs = LogStore::new();
        let runtime = RuntimeStore::new();
        let telemetry = TelemetrySnapshot::default();
        let model = SessionViewModel::new(&board, &logs, &runtime, &telemetry, Instant::now());
        let mut state = AppState {
            page: Page::Runtimes,
            navigation: NavigationLevel::Page,
            ..AppState::for_mode(SessionMode::Simulation)
        };

        state.sync(&model);
        assert_eq!(model.runtimes[state.runtime_cursor].id, "alpha");
        state.handle_key(key(KeyCode::Down), &model);
        assert_eq!(model.runtimes[state.runtime_cursor].id, "alpha");
        state.handle_key(key(KeyCode::Enter), &model);
        assert_eq!(state.runtime_detail_id.as_deref(), Some("alpha"));
    }

    #[test]
    fn disappearing_frozen_runtime_returns_to_the_runtime_list() {
        let mut board = BoardSnapshot::default();
        for id in ["alpha", "beta"] {
            board.participants.insert(
                id.to_string(),
                ParticipantStatus::new(id, ParticipantKind::Service, ParticipantState::Ready),
            );
        }
        let logs = LogStore::new();
        let runtime = RuntimeStore::new();
        let telemetry = TelemetrySnapshot::default();
        let mut state = AppState {
            page: Page::Runtimes,
            navigation: NavigationLevel::Page,
            runtime_detail_id: Some("alpha".to_string()),
            ..AppState::default()
        };

        board.participants.remove("alpha");
        let model = SessionViewModel::new(&board, &logs, &runtime, &telemetry, Instant::now());
        state.sync(&model);

        assert!(state.runtime_detail_id.is_none());
        assert_eq!(model.runtimes[state.runtime_cursor].id, "beta");
    }
}
