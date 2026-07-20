//! Pure five-page terminal navigation and input handling.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::tui::DisplayAction;
use crate::tui::view_model::SessionViewModel;
use crate::tui::visibility::is_internal_id;
use phoxal_cli_core::session::SessionMode;
use phoxal_cli_core::session::TopicMetric;
use phoxal_cli_core::session::stores::log::DisplayedLine;
use phoxal_cli_core::session::stores::telemetry::DEFAULT_FRESHNESS_TTL;
use phoxal_cli_core::session::{LogSeverity, ParticipantStatus};

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
    pub runtime_topic_offset: usize,
    runtime_topic_visible_rows: usize,
    pub simulation: bool,
    pub log_source_filter: LogSourceFilter,
    pub log_filter_cursor: usize,
    pub log_text_filter: String,
    pub log_runtime_filter: String,
    pub log_severity: SeverityFilter,
    pub log_scroll: usize,
    pub log_follow: bool,
    pub log_pause_anchor: Option<std::time::SystemTime>,
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
            runtime_topic_offset: 0,
            runtime_topic_visible_rows: 1,
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
                self.runtime_topic_offset = self
                    .runtime_topic_offset
                    .min(self.runtime_topic_max_offset(model.runtime_topic_count(detail_id)));
            } else {
                self.runtime_detail_id = None;
                self.runtime_topic_offset = 0;
                self.runtime_topic_visible_rows = 1;
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

    pub(super) fn set_runtime_topic_viewport(&mut self, row_count: usize, visible_rows: usize) {
        self.runtime_topic_visible_rows = visible_rows.max(1);
        self.runtime_topic_offset = self
            .runtime_topic_offset
            .min(self.runtime_topic_max_offset(row_count));
    }

    fn runtime_topic_max_offset(&self, row_count: usize) -> usize {
        row_count.saturating_sub(self.runtime_topic_visible_rows)
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
        self.runtime_topic_offset = 0;
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
                self.runtime_topic_offset = 0;
                self.runtime_topic_visible_rows = 1;
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
        self.runtime_topic_offset = 0;
        self.runtime_topic_visible_rows = 1;
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
            KeyCode::Enter if self.runtime_detail_id.is_none() => {
                self.runtime_detail_id =
                    self.selected_runtime(model).map(|status| status.id.clone());
                self.runtime_cursor_id.clone_from(&self.runtime_detail_id);
                self.runtime_topic_offset = 0;
            }
            KeyCode::Up => {
                if self.runtime_detail_id.is_some() {
                    self.runtime_topic_offset = self.runtime_topic_offset.saturating_sub(1);
                } else {
                    self.move_runtime_cursor(model, -1);
                }
            }
            KeyCode::Down => {
                if self.runtime_detail_id.is_some() {
                    let max_offset = self.runtime_detail_id.as_deref().map_or(0, |id| {
                        self.runtime_topic_max_offset(model.runtime_topic_count(id))
                    });
                    self.runtime_topic_offset =
                        self.runtime_topic_offset.saturating_add(1).min(max_offset);
                } else {
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
            let participant = CaseInsensitiveNeedle::new(&self.log_runtime_filter);
            let text = CaseInsensitiveNeedle::new(&self.log_text_filter);
            self.log_pause_anchor = Some(
                model
                    .logs
                    .lines()
                    .filter(|line| self.log_line_matches(line, model, &participant, &text))
                    .map(|line| line.event_time)
                    .max()
                    .unwrap_or_else(std::time::SystemTime::now),
            );
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
                        .is_none_or(|anchor| line.event_time <= anchor)
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
mod tests;
