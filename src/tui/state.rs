//! Interactive TUI state and key handling - deliberately free of any
//! ratatui/crossterm *rendering* concerns so it is unit-testable without a
//! terminal. Only [`AppState::handle_key`]'s input type
//! ([`crossterm::event::KeyEvent`]) ties this to crossterm; construct one
//! directly in a test rather than driving a real terminal.
//!
//! # Per-panel state (Target design part 6)
//!
//! Each detail-surface panel owns its OWN cursor/filter/scroll state
//! ([`NavigatorState`], [`OverviewState`], [`LogsState`], [`TrafficState`],
//! [`DevicesState`], [`ResourcesState`]) rather than one flat struct with
//! every field always present - the old shape let a joypad cursor coexist
//! with a traffic sort even though only one panel is ever visible at a time.
//! [`AppState`] composes these; [`AppState::panel`] (a [`Panel`], never a
//! participant-id string check - see `tui::panel`) says which one is active.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::display::DisplayAction;
use crate::stores::telemetry_store::HostPoint;
use crate::supervisor::BoardSnapshot;
use crate::telemetry::{HostSample, TelemetrySnapshot};
use crate::tui::diagnostics::DiagnosticsFilter;
use crate::tui::groups::{Group, GroupSection, build_groups, suggested_participant};
use crate::tui::panel::{Panel, panels_for};

/// Sort key for the `tool-router` Traffic table (`s` cycles through these in
/// order); default is the busiest-first view an operator opens the tab to
/// see.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TrafficSort {
    #[default]
    Rate,
    Topic,
    Producer,
    Status,
}

impl TrafficSort {
    #[must_use]
    const fn cycle(self) -> Self {
        match self {
            Self::Rate => Self::Topic,
            Self::Topic => Self::Producer,
            Self::Producer => Self::Status,
            Self::Status => Self::Rate,
        }
    }

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Rate => "rate",
            Self::Topic => "topic",
            Self::Producer => "producer",
            Self::Status => "status",
        }
    }
}

/// The Resources tab's rolling-history display window (`w` cycles through
/// these) - a trailing slice of `TelemetrySnapshot::host_history` by receive
/// time, not a separate sample buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ResourcesRange {
    ThirtySeconds,
    #[default]
    OneMinute,
    TwoMinutes,
}

impl ResourcesRange {
    #[must_use]
    const fn cycle(self) -> Self {
        match self {
            Self::ThirtySeconds => Self::OneMinute,
            Self::OneMinute => Self::TwoMinutes,
            Self::TwoMinutes => Self::ThirtySeconds,
        }
    }

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::ThirtySeconds => "30s",
            Self::OneMinute => "1m",
            Self::TwoMinutes => "2m",
        }
    }

    #[must_use]
    pub const fn seconds(self) -> u64 {
        match self {
            Self::ThirtySeconds => 30,
            Self::OneMinute => 60,
            Self::TwoMinutes => 120,
        }
    }
}

/// One flattened navigator row: a group heading (not selectable) or a
/// participant id (selectable).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NavRow {
    Header(Group),
    Participant(String),
}

/// What the right pane shows: the home Overview, or a specific runtime's
/// detail surface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum View {
    Home,
    Diagnostics,
    Runtime(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Navigator,
    Detail,
}

/// The home navigator's own state: the flattened rows, cursor, and filter
/// buffer - see the module docs on per-panel state.
#[derive(Debug, Clone, Default)]
pub struct NavigatorState {
    pub rows: Vec<NavRow>,
    pub cursor: usize,
    pub filter: String,
    pub filtering: bool,
    /// Becomes `true` the first time the user moves the cursor themselves;
    /// until then, `sync` keeps steering the cursor at the suggested
    /// (failed/degraded, else starting) row so the navigator opens already
    /// pointed at what needs attention.
    user_moved_cursor: bool,
}

/// The Overview panel's own state: just its scroll offset.
#[derive(Debug, Clone, Copy, Default)]
pub struct OverviewState {
    pub scroll: usize,
}

/// The global Diagnostics tab's local query, severity, and scroll state.
#[derive(Debug, Clone, Default)]
pub struct DiagnosticsState {
    pub(crate) filter: String,
    pub(crate) filtering: bool,
    pub(crate) severity: DiagnosticsFilter,
    /// Rows scrolled back from the newest matching diagnostic.
    pub(crate) scroll: usize,
}

/// The Logs panel's own state.
#[derive(Debug, Clone)]
pub struct LogsState {
    pub filter: String,
    pub filtering: bool,
    pub scroll: usize,
    pub follow: bool,
}

impl Default for LogsState {
    fn default() -> Self {
        Self {
            filter: String::new(),
            filtering: false,
            scroll: 0,
            follow: true,
        }
    }
}

/// The router Traffic panel's own state.
#[derive(Debug, Clone, Default)]
pub struct TrafficState {
    pub sort: TrafficSort,
    pub filter: String,
    pub filtering: bool,
    pub scroll: usize,
}

/// The joypad Devices panel's own state.
#[derive(Debug, Clone, Copy, Default)]
pub struct DevicesState {
    /// Cursor into `telemetry.joypad.available` for the `tool-joypad`
    /// Devices panel's pure list navigation - purely local UI state, NOT the
    /// actual selection (that comes from the tool's own `Devices` ack; see
    /// `crate::telemetry::JoypadDevicesSample`'s docs).
    pub cursor: usize,
}

/// The `tool-telemetry` Resources panel's own state: the display window and
/// whether the operator has paused the live feed (`p`), in which case the
/// values captured at the moment of pausing are shown instead of the
/// still-arriving live ones until resumed.
#[derive(Debug, Clone, Default)]
pub struct ResourcesState {
    pub range: ResourcesRange,
    pub paused: bool,
    frozen_host: Option<HostSample>,
    frozen_history: Vec<HostPoint>,
}

impl ResourcesState {
    /// The host sample to render: the frozen one while paused, else `live`.
    #[must_use]
    pub fn display_host<'a>(&'a self, live: Option<&'a HostSample>) -> Option<&'a HostSample> {
        if self.paused {
            self.frozen_host.as_ref()
        } else {
            live
        }
    }

    /// The history to render: the frozen one while paused, else `live`.
    #[must_use]
    pub fn display_history<'a>(&'a self, live: &'a [HostPoint]) -> &'a [HostPoint] {
        if self.paused {
            &self.frozen_history
        } else {
            live
        }
    }

    fn toggle_pause(&mut self, host: Option<HostSample>, history: &[HostPoint]) {
        self.paused = !self.paused;
        if self.paused {
            self.frozen_host = host;
            self.frozen_history = history.to_vec();
        }
    }
}

/// The full interactive state of the TUI shell, rebuilt against the latest
/// [`BoardSnapshot`] every redraw ([`AppState::sync`]) and mutated by
/// [`AppState::handle_key`].
#[derive(Debug, Clone)]
pub struct AppState {
    simulation: bool,
    pub navigator: NavigatorState,
    pub view: View,
    pub panel: Panel,
    pub focus: Focus,
    pub show_help: bool,
    pub overview: OverviewState,
    pub diagnostics: DiagnosticsState,
    pub logs: LogsState,
    pub traffic: TrafficState,
    pub devices: DevicesState,
    pub resources: ResourcesState,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            simulation: false,
            navigator: NavigatorState::default(),
            view: View::Home,
            panel: Panel::default(),
            focus: Focus::Navigator,
            show_help: false,
            overview: OverviewState::default(),
            diagnostics: DiagnosticsState::default(),
            logs: LogsState::default(),
            traffic: TrafficState::default(),
            devices: DevicesState::default(),
            resources: ResourcesState::default(),
        }
    }
}

impl AppState {
    #[cfg(test)]
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn for_mode(mode: &str) -> Self {
        Self {
            simulation: mode == "simulation",
            ..Self::default()
        }
    }

    /// Recompute the flattened navigator rows against the latest board, and -
    /// until the user has navigated by hand - keep the cursor on the
    /// suggested row (design doc: "if any entity is failed/degraded,
    /// highlight it as the suggested selection; else suggest the first
    /// still-starting entity").
    pub fn sync(&mut self, board: &BoardSnapshot) {
        let filter = self.navigator.filter.to_lowercase();
        let sections = build_groups(board, &filter, self.simulation);
        self.navigator.rows = flatten(&sections);
        if self.navigator.cursor >= self.navigator.rows.len() {
            self.navigator.cursor = self.navigator.rows.len().saturating_sub(1);
        }
        if !matches!(
            self.navigator.rows.get(self.navigator.cursor),
            Some(NavRow::Participant(_))
        ) {
            self.navigator.cursor = first_selectable(&self.navigator.rows).unwrap_or(0);
        }
        if !self.navigator.user_moved_cursor
            && let Some(suggested) = suggested_participant(board)
            && let Some(index) = self
                .navigator
                .rows
                .iter()
                .position(|row| matches!(row, NavRow::Participant(id) if id == &suggested.id))
        {
            self.navigator.cursor = index;
        }
    }

    #[must_use]
    pub fn selected_id(&self) -> Option<&str> {
        match self.navigator.rows.get(self.navigator.cursor) {
            Some(NavRow::Participant(id)) => Some(id.as_str()),
            _ => None,
        }
    }

    /// Handle one key event, returning what the supervisor loop must act on
    /// (`DisplayAction::None` for anything purely internal to the TUI).
    /// `telemetry` is only consulted by the detail focus's bespoke panels
    /// (the Devices list's cursor bound and which device id `↵` resolves to,
    /// and the Resources pause snapshot) - navigation, filtering, and every
    /// other key path ignore it.
    pub fn handle_key(
        &mut self,
        key: KeyEvent,
        board: &BoardSnapshot,
        telemetry: &TelemetrySnapshot,
    ) -> DisplayAction {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return DisplayAction::Quit;
        }
        if self.is_filtering() {
            return self.handle_filter_key(key);
        }
        if key.code == KeyCode::Char('?') {
            self.show_help = !self.show_help;
            return DisplayAction::None;
        }
        if self.show_help {
            // Any other key dismisses the help overlay rather than reaching
            // the view underneath - avoids a stray navigation while reading it.
            self.show_help = false;
            return DisplayAction::None;
        }
        if key.code == KeyCode::Char('d') {
            if self.view == View::Diagnostics {
                self.view = View::Home;
                self.focus = Focus::Navigator;
            } else {
                self.open_diagnostics();
            }
            return DisplayAction::None;
        }
        if self.view == View::Diagnostics {
            return self.handle_diagnostics_key(key);
        }
        match self.focus {
            Focus::Navigator => self.handle_navigator_key(key, board),
            Focus::Detail => self.handle_detail_key(key, telemetry),
        }
    }

    /// Whether ANY panel's filter buffer is currently accepting typed input -
    /// public so `tui::render`'s footer can show the filter-mode hint without
    /// re-deriving this from the three per-panel flags itself. Exactly one of
    /// `navigator`/`logs`/`traffic`'s `filtering` flags is ever `true` at
    /// once, since only one panel is visible at a time and `stop_filtering`
    /// clears all three together.
    #[must_use]
    pub fn is_filtering(&self) -> bool {
        self.navigator.filtering
            || self.diagnostics.filtering
            || self.logs.filtering
            || self.traffic.filtering
    }

    fn stop_filtering(&mut self) {
        self.navigator.filtering = false;
        self.diagnostics.filtering = false;
        self.logs.filtering = false;
        self.traffic.filtering = false;
    }

    /// The filter buffer the currently active filter mode types into - the
    /// navigator's own filter for every case except the Logs/Traffic panels,
    /// each of which gets its own buffer so switching panels never clobbers
    /// another panel's filter (matches the pre-split behavior exactly).
    fn active_filter_buffer_mut(&mut self) -> &mut String {
        if self.view == View::Diagnostics {
            &mut self.diagnostics.filter
        } else if self.focus == Focus::Detail && self.panel == Panel::Logs {
            &mut self.logs.filter
        } else if self.focus == Focus::Detail && self.panel == Panel::Traffic {
            &mut self.traffic.filter
        } else {
            &mut self.navigator.filter
        }
    }

    fn handle_filter_key(&mut self, key: KeyEvent) -> DisplayAction {
        match key.code {
            KeyCode::Enter | KeyCode::Esc => self.stop_filtering(),
            KeyCode::Backspace => {
                self.active_filter_buffer_mut().pop();
            }
            KeyCode::Char(character) => self.active_filter_buffer_mut().push(character),
            _ => {}
        }
        DisplayAction::None
    }

    /// Open the session-wide Diagnostics tab. Used both by the `d` shortcut
    /// and by the display when a fatal diagnostic arrives during startup.
    pub(crate) fn open_diagnostics(&mut self) {
        self.view = View::Diagnostics;
        self.focus = Focus::Detail;
        self.diagnostics.scroll = 0;
    }

    fn handle_diagnostics_key(&mut self, key: KeyEvent) -> DisplayAction {
        match key.code {
            KeyCode::Esc | KeyCode::Left => {
                self.view = View::Home;
                self.focus = Focus::Navigator;
            }
            KeyCode::Up => {
                self.diagnostics.scroll = self.diagnostics.scroll.saturating_add(1);
            }
            KeyCode::Down => {
                self.diagnostics.scroll = self.diagnostics.scroll.saturating_sub(1);
            }
            KeyCode::Char('s') => {
                self.diagnostics.severity = self.diagnostics.severity.cycle();
                self.diagnostics.scroll = 0;
            }
            KeyCode::Char('/') => self.diagnostics.filtering = true,
            _ => {}
        }
        DisplayAction::None
    }

    fn handle_navigator_key(&mut self, key: KeyEvent, _board: &BoardSnapshot) -> DisplayAction {
        match key.code {
            KeyCode::Char('q') => return DisplayAction::Quit,
            KeyCode::Char('/') => self.navigator.filtering = true,
            KeyCode::Up => self.move_cursor(-1),
            KeyCode::Down => self.move_cursor(1),
            KeyCode::Enter => {
                if let Some(id) = self.selected_id() {
                    self.view = View::Runtime(id.to_string());
                    self.panel = Panel::Overview;
                    self.overview.scroll = 0;
                    self.logs.scroll = 0;
                    self.devices.cursor = 0;
                    self.traffic.scroll = 0;
                    self.focus = Focus::Detail;
                }
            }
            KeyCode::Char('r') => {
                if let Some(id) = self.selected_id() {
                    return DisplayAction::Restart(id.to_string());
                }
            }
            _ => {}
        }
        DisplayAction::None
    }

    fn handle_detail_key(&mut self, key: KeyEvent, telemetry: &TelemetrySnapshot) -> DisplayAction {
        let View::Runtime(id) = &self.view else {
            self.focus = Focus::Navigator;
            return DisplayAction::None;
        };
        let panels = panels_for(id);
        let on_traffic = self.panel == Panel::Traffic;
        let on_devices = self.panel == Panel::Devices;
        let on_resources = self.panel == Panel::Resources;
        match key.code {
            KeyCode::Esc => {
                self.view = View::Home;
                self.focus = Focus::Navigator;
            }
            KeyCode::Left => self.panel = cycle_panel(&panels, self.panel, -1),
            KeyCode::Right => self.panel = cycle_panel(&panels, self.panel, 1),
            KeyCode::Up if on_devices => self.move_device_cursor(-1, telemetry),
            KeyCode::Down if on_devices => self.move_device_cursor(1, telemetry),
            KeyCode::Up if on_traffic => {
                self.traffic.scroll = self.traffic.scroll.saturating_sub(1);
            }
            KeyCode::Down if on_traffic => {
                self.traffic.scroll = self.traffic.scroll.saturating_add(1);
            }
            KeyCode::Up => self.scroll(-1),
            KeyCode::Down => self.scroll(1),
            KeyCode::Enter if on_devices => {
                if let Some(device) = telemetry
                    .joypad
                    .as_ref()
                    .and_then(|devices| devices.value.available.get(self.devices.cursor))
                {
                    return DisplayAction::JoypadConnect(device.id.clone());
                }
            }
            KeyCode::Char('r') if on_devices => return DisplayAction::JoypadRescan,
            KeyCode::Char('s') if on_traffic => {
                self.traffic.sort = self.traffic.sort.cycle();
            }
            KeyCode::Char('p') if on_resources => {
                let host = telemetry.host.as_ref().map(|sample| sample.value);
                self.resources.toggle_pause(host, &telemetry.host_history);
            }
            KeyCode::Char('w') if on_resources => {
                self.resources.range = self.resources.range.cycle();
            }
            KeyCode::Char('f') if self.panel == Panel::Logs => {
                self.logs.follow = !self.logs.follow;
            }
            KeyCode::Char('/') if self.panel == Panel::Logs => {
                self.logs.filtering = true;
            }
            KeyCode::Char('/') if on_traffic => {
                self.traffic.filtering = true;
            }
            _ => {}
        }
        DisplayAction::None
    }

    /// Move the joypad Devices list cursor by `delta`, clamped to the
    /// current device count (0 if the list is empty or not yet observed -
    /// `Enter` then simply has nothing to resolve, see `handle_detail_key`).
    fn move_device_cursor(&mut self, delta: isize, telemetry: &TelemetrySnapshot) {
        let len = telemetry
            .joypad
            .as_ref()
            .map_or(0, |devices| devices.value.available.len());
        if len == 0 {
            self.devices.cursor = 0;
            return;
        }
        let next = (self.devices.cursor as isize + delta).rem_euclid(len as isize) as usize;
        self.devices.cursor = next;
    }

    fn move_cursor(&mut self, delta: isize) {
        self.navigator.user_moved_cursor = true;
        let selectable = selectable_indices(&self.navigator.rows);
        if selectable.is_empty() {
            return;
        }
        let current_pos = selectable
            .iter()
            .position(|&index| index == self.navigator.cursor)
            .unwrap_or(0);
        let len = selectable.len() as isize;
        let next = (current_pos as isize + delta).rem_euclid(len) as usize;
        self.navigator.cursor = selectable[next];
    }

    /// Scroll the currently visible scrollable panel (Overview or Logs only;
    /// Traffic/Devices have their own dedicated `Up`/`Down` handling above,
    /// and Resources has no scroll, only `p`/`w`).
    fn scroll(&mut self, delta: isize) {
        let target = match self.panel {
            Panel::Logs => &mut self.logs.scroll,
            Panel::Overview => &mut self.overview.scroll,
            Panel::Traffic | Panel::Devices | Panel::Resources => return,
        };
        if delta.is_negative() {
            *target = target.saturating_sub(delta.unsigned_abs());
        } else {
            *target = target.saturating_add(delta.unsigned_abs());
        }
    }
}

fn flatten(sections: &[GroupSection]) -> Vec<NavRow> {
    let mut rows = Vec::new();
    for section in sections {
        rows.push(NavRow::Header(section.group));
        for id in &section.ids {
            rows.push(NavRow::Participant(id.clone()));
        }
    }
    rows
}

fn selectable_indices(rows: &[NavRow]) -> Vec<usize> {
    rows.iter()
        .enumerate()
        .filter_map(|(index, row)| matches!(row, NavRow::Participant(_)).then_some(index))
        .collect()
}

fn first_selectable(rows: &[NavRow]) -> Option<usize> {
    selectable_indices(rows).into_iter().next()
}

fn cycle_panel(panels: &[Panel], current: Panel, delta: isize) -> Panel {
    let Some(index) = panels.iter().position(|panel| *panel == current) else {
        return panels.first().copied().unwrap_or(Panel::Overview);
    };
    let len = panels.len() as isize;
    let next = ((index as isize + delta).rem_euclid(len)) as usize;
    panels[next]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::launch_plan::{SITE_TOOL_JOYPAD, SITE_TOOL_ROUTER};
    use crate::participant_kind::ParticipantKind;
    use crate::stores::telemetry_store::Timestamped;
    use crate::supervisor::{ParticipantState, ParticipantStatus};
    use crossterm::event::{KeyEventKind, KeyEventState};
    use std::time::Instant;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    fn ctrl_c() -> KeyEvent {
        KeyEvent {
            code: KeyCode::Char('c'),
            modifiers: KeyModifiers::CONTROL,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    fn board_with(entries: &[(&str, ParticipantKind, ParticipantState)]) -> BoardSnapshot {
        let mut board = BoardSnapshot::default();
        for (id, kind, state) in entries {
            board.participants.insert(
                (*id).to_string(),
                ParticipantStatus::new(*id, *kind, *state),
            );
        }
        board
    }

    #[test]
    fn ctrl_c_quits_from_any_focus() {
        let mut state = AppState::new();
        let board = board_with(&[("drive", ParticipantKind::Service, ParticipantState::Ready)]);
        state.sync(&board);
        assert!(matches!(
            state.handle_key(ctrl_c(), &board, &TelemetrySnapshot::default()),
            DisplayAction::Quit
        ));
    }

    #[test]
    fn q_quits_in_the_navigator() {
        let mut state = AppState::new();
        let board = board_with(&[("drive", ParticipantKind::Service, ParticipantState::Ready)]);
        state.sync(&board);
        assert!(matches!(
            state.handle_key(
                key(KeyCode::Char('q')),
                &board,
                &TelemetrySnapshot::default()
            ),
            DisplayAction::Quit
        ));
    }

    #[test]
    fn q_is_local_text_outside_the_top_level_navigator() {
        let mut state = AppState::new();
        let board = board_with(&[("drive", ParticipantKind::Service, ParticipantState::Ready)]);
        state.sync(&board);
        state.handle_key(key(KeyCode::Enter), &board, &TelemetrySnapshot::default());
        assert_eq!(state.focus, Focus::Detail);
        assert_eq!(
            state.handle_key(
                key(KeyCode::Char('q')),
                &board,
                &TelemetrySnapshot::default()
            ),
            DisplayAction::None
        );

        state.open_diagnostics();
        assert_eq!(
            state.handle_key(
                key(KeyCode::Char('q')),
                &board,
                &TelemetrySnapshot::default()
            ),
            DisplayAction::None
        );
    }

    #[test]
    fn diagnostics_is_global_and_d_toggles_back_to_home() {
        let mut state = AppState::new();
        let board = BoardSnapshot::default();
        state.handle_key(
            key(KeyCode::Char('d')),
            &board,
            &TelemetrySnapshot::default(),
        );
        assert_eq!(state.view, View::Diagnostics);
        assert_eq!(state.focus, Focus::Detail);

        state.handle_key(
            key(KeyCode::Char('d')),
            &board,
            &TelemetrySnapshot::default(),
        );
        assert_eq!(state.view, View::Home);
        assert_eq!(state.focus, Focus::Navigator);
    }

    #[test]
    fn diagnostics_has_independent_severity_filter_and_scroll_controls() {
        let mut state = AppState::new();
        let board = BoardSnapshot::default();
        state.open_diagnostics();
        state.handle_key(
            key(KeyCode::Char('s')),
            &board,
            &TelemetrySnapshot::default(),
        );
        assert_eq!(state.diagnostics.severity, DiagnosticsFilter::Warnings);
        state.handle_key(key(KeyCode::Up), &board, &TelemetrySnapshot::default());
        assert_eq!(state.diagnostics.scroll, 1);
        state.handle_key(
            key(KeyCode::Char('/')),
            &board,
            &TelemetrySnapshot::default(),
        );
        state.handle_key(
            key(KeyCode::Char('w')),
            &board,
            &TelemetrySnapshot::default(),
        );
        assert_eq!(state.diagnostics.filter, "w");
        assert!(state.navigator.filter.is_empty());
    }

    #[test]
    fn cursor_starts_on_the_suggested_failed_row() {
        let mut state = AppState::new();
        let board = board_with(&[
            ("healthy", ParticipantKind::Service, ParticipantState::Ready),
            ("broken", ParticipantKind::Service, ParticipantState::Failed),
        ]);
        state.sync(&board);
        assert_eq!(state.selected_id(), Some("broken"));
    }

    #[test]
    fn up_down_navigation_skips_group_headers_and_wraps() {
        let mut state = AppState::new();
        let board = board_with(&[
            ("drive", ParticipantKind::Service, ParticipantState::Ready),
            ("wheel", ParticipantKind::Driver, ParticipantState::Ready),
        ]);
        state.sync(&board);
        let first = state.selected_id().map(str::to_string);
        state.handle_key(key(KeyCode::Down), &board, &TelemetrySnapshot::default());
        let second = state.selected_id().map(str::to_string);
        assert_ne!(
            first, second,
            "moving down must land on a different participant"
        );
        state.handle_key(key(KeyCode::Down), &board, &TelemetrySnapshot::default());
        assert_eq!(
            state.selected_id().map(str::to_string),
            first,
            "navigation must wrap back to the first selectable row"
        );
    }

    #[test]
    fn enter_opens_detail_and_moves_focus() {
        let mut state = AppState::new();
        let board = board_with(&[("drive", ParticipantKind::Service, ParticipantState::Ready)]);
        state.sync(&board);
        state.handle_key(key(KeyCode::Enter), &board, &TelemetrySnapshot::default());
        assert_eq!(state.view, View::Runtime("drive".to_string()));
        assert_eq!(state.focus, Focus::Detail);
        assert_eq!(state.panel, Panel::Overview);
    }

    #[test]
    fn esc_from_detail_returns_home_and_focus_to_navigator() {
        let mut state = AppState::new();
        let board = board_with(&[("drive", ParticipantKind::Service, ParticipantState::Ready)]);
        state.sync(&board);
        state.handle_key(key(KeyCode::Enter), &board, &TelemetrySnapshot::default());
        state.handle_key(key(KeyCode::Esc), &board, &TelemetrySnapshot::default());
        assert_eq!(state.view, View::Home);
        assert_eq!(state.focus, Focus::Navigator);
    }

    #[test]
    fn r_restart_emits_a_restart_action_for_the_selected_row() {
        let mut state = AppState::new();
        let board = board_with(&[("drive", ParticipantKind::Service, ParticipantState::Ready)]);
        state.sync(&board);
        let action = state.handle_key(
            key(KeyCode::Char('r')),
            &board,
            &TelemetrySnapshot::default(),
        );
        assert!(matches!(action, DisplayAction::Restart(id) if id == "drive"));
    }

    #[test]
    fn panel_set_includes_the_bespoke_panel_only_for_hardcoded_system_tools() {
        assert_eq!(
            panels_for("tool-router"),
            vec![Panel::Overview, Panel::Logs, Panel::Traffic]
        );
        assert_eq!(panels_for("drive"), vec![Panel::Overview, Panel::Logs]);
    }

    #[test]
    fn left_right_cycles_panels_and_wraps() {
        let mut state = AppState::new();
        let board = board_with(&[(
            SITE_TOOL_ROUTER,
            ParticipantKind::Tool,
            ParticipantState::Ready,
        )]);
        state.sync(&board);
        state.handle_key(key(KeyCode::Enter), &board, &TelemetrySnapshot::default());
        assert_eq!(state.panel, Panel::Overview);
        state.handle_key(key(KeyCode::Right), &board, &TelemetrySnapshot::default());
        assert_eq!(state.panel, Panel::Logs);
        state.handle_key(key(KeyCode::Right), &board, &TelemetrySnapshot::default());
        assert_eq!(state.panel, Panel::Traffic);
        state.handle_key(key(KeyCode::Right), &board, &TelemetrySnapshot::default());
        assert_eq!(state.panel, Panel::Overview, "must wrap back around");
    }

    #[test]
    fn slash_enters_filter_mode_and_typed_characters_accumulate() {
        let mut state = AppState::new();
        let board = board_with(&[("drive", ParticipantKind::Service, ParticipantState::Ready)]);
        state.sync(&board);
        state.handle_key(
            key(KeyCode::Char('/')),
            &board,
            &TelemetrySnapshot::default(),
        );
        assert!(state.navigator.filtering);
        state.handle_key(
            key(KeyCode::Char('d')),
            &board,
            &TelemetrySnapshot::default(),
        );
        state.handle_key(
            key(KeyCode::Char('r')),
            &board,
            &TelemetrySnapshot::default(),
        );
        assert_eq!(state.navigator.filter, "dr");
        state.handle_key(key(KeyCode::Enter), &board, &TelemetrySnapshot::default());
        assert!(!state.navigator.filtering);
    }

    #[test]
    fn help_toggle_does_not_leak_into_navigation() {
        let mut state = AppState::new();
        let board = board_with(&[("drive", ParticipantKind::Service, ParticipantState::Ready)]);
        state.sync(&board);
        state.handle_key(
            key(KeyCode::Char('?')),
            &board,
            &TelemetrySnapshot::default(),
        );
        assert!(state.show_help);
        // Any other key dismisses help rather than navigating.
        state.handle_key(key(KeyCode::Down), &board, &TelemetrySnapshot::default());
        assert!(!state.show_help);
    }

    fn joypad_telemetry() -> TelemetrySnapshot {
        let mut telemetry = TelemetrySnapshot::default();
        telemetry.joypad = Some(Timestamped {
            value: crate::telemetry::JoypadDevicesSample {
                available: vec![
                    crate::telemetry::JoypadDevice {
                        id: "pad-a".to_string(),
                        name: "Pad A".to_string(),
                        connected: true,
                    },
                    crate::telemetry::JoypadDevice {
                        id: "pad-b".to_string(),
                        name: "Pad B".to_string(),
                        connected: true,
                    },
                ],
                selected: None,
                last_error: None,
            },
            received_at: Instant::now(),
        });
        telemetry
    }

    fn open_joypad_devices_panel(state: &mut AppState, board: &BoardSnapshot) {
        state.sync(board);
        state.handle_key(key(KeyCode::Enter), board, &TelemetrySnapshot::default());
        // Overview -> Logs -> Devices.
        state.handle_key(key(KeyCode::Right), board, &TelemetrySnapshot::default());
        state.handle_key(key(KeyCode::Right), board, &TelemetrySnapshot::default());
        assert_eq!(state.panel, Panel::Devices);
    }

    #[test]
    fn joypad_enter_publishes_connect_for_the_device_under_the_cursor() {
        let mut state = AppState::new();
        let board = board_with(&[(
            SITE_TOOL_JOYPAD,
            ParticipantKind::Tool,
            ParticipantState::Ready,
        )]);
        open_joypad_devices_panel(&mut state, &board);
        let telemetry = joypad_telemetry();

        // Cursor starts at 0 (pad-a); down moves it to pad-b.
        let action = state.handle_key(key(KeyCode::Down), &board, &telemetry);
        assert!(matches!(action, DisplayAction::None));
        assert_eq!(state.devices.cursor, 1);

        let action = state.handle_key(key(KeyCode::Enter), &board, &telemetry);
        assert!(matches!(action, DisplayAction::JoypadConnect(id) if id == "pad-b"));
    }

    #[test]
    fn joypad_r_publishes_rescan() {
        let mut state = AppState::new();
        let board = board_with(&[(
            SITE_TOOL_JOYPAD,
            ParticipantKind::Tool,
            ParticipantState::Ready,
        )]);
        open_joypad_devices_panel(&mut state, &board);
        let telemetry = joypad_telemetry();

        let action = state.handle_key(key(KeyCode::Char('r')), &board, &telemetry);
        assert!(matches!(action, DisplayAction::JoypadRescan));
    }

    #[test]
    fn joypad_cursor_wraps_and_stays_zero_with_no_devices() {
        let mut state = AppState::new();
        let board = board_with(&[(
            SITE_TOOL_JOYPAD,
            ParticipantKind::Tool,
            ParticipantState::Ready,
        )]);
        open_joypad_devices_panel(&mut state, &board);

        // No devices observed yet: the cursor stays at 0, Enter has nothing
        // to resolve to (a `Connect` action never fires against an empty
        // list).
        let empty = TelemetrySnapshot::default();
        state.handle_key(key(KeyCode::Down), &board, &empty);
        assert_eq!(state.devices.cursor, 0);
        let action = state.handle_key(key(KeyCode::Enter), &board, &empty);
        assert!(matches!(action, DisplayAction::None));

        // Two devices: down wraps from the last back to the first.
        let telemetry = joypad_telemetry();
        state.handle_key(key(KeyCode::Down), &board, &telemetry);
        assert_eq!(state.devices.cursor, 1);
        state.handle_key(key(KeyCode::Down), &board, &telemetry);
        assert_eq!(
            state.devices.cursor, 0,
            "must wrap back to the first device"
        );
    }

    #[test]
    fn router_traffic_panel_slash_routes_into_the_traffic_filter_buffer() {
        let mut state = AppState::new();
        let board = board_with(&[(
            SITE_TOOL_ROUTER,
            ParticipantKind::Tool,
            ParticipantState::Ready,
        )]);
        state.sync(&board);
        state.handle_key(key(KeyCode::Enter), &board, &TelemetrySnapshot::default());
        // Overview -> Logs -> Traffic.
        state.handle_key(key(KeyCode::Right), &board, &TelemetrySnapshot::default());
        state.handle_key(key(KeyCode::Right), &board, &TelemetrySnapshot::default());
        assert_eq!(state.panel, Panel::Traffic);

        state.handle_key(
            key(KeyCode::Char('/')),
            &board,
            &TelemetrySnapshot::default(),
        );
        assert!(state.traffic.filtering);
        state.handle_key(
            key(KeyCode::Char('x')),
            &board,
            &TelemetrySnapshot::default(),
        );
        assert_eq!(state.traffic.filter, "x");
        assert!(
            state.navigator.filter.is_empty(),
            "typing in the Traffic filter must not leak into the navigator filter"
        );
    }

    #[test]
    fn router_traffic_panel_s_cycles_sort() {
        let mut state = AppState::new();
        let board = board_with(&[(
            SITE_TOOL_ROUTER,
            ParticipantKind::Tool,
            ParticipantState::Ready,
        )]);
        state.sync(&board);
        state.handle_key(key(KeyCode::Enter), &board, &TelemetrySnapshot::default());
        state.handle_key(key(KeyCode::Right), &board, &TelemetrySnapshot::default());
        state.handle_key(key(KeyCode::Right), &board, &TelemetrySnapshot::default());
        assert_eq!(state.traffic.sort, TrafficSort::Rate);
        state.handle_key(
            key(KeyCode::Char('s')),
            &board,
            &TelemetrySnapshot::default(),
        );
        assert_eq!(state.traffic.sort, TrafficSort::Topic);
    }

    /// The structural point of the per-panel split: opening the joypad
    /// Devices panel and moving its cursor must never touch the Traffic
    /// panel's sort/filter state, and vice versa - the old flat `AppState`
    /// let both fields be set simultaneously with no way to tell which
    /// applied; now they simply live in different structs.
    #[test]
    fn devices_cursor_and_traffic_sort_are_isolated_per_panel_state() {
        let mut state = AppState::new();
        let board = board_with(&[(
            SITE_TOOL_JOYPAD,
            ParticipantKind::Tool,
            ParticipantState::Ready,
        )]);
        open_joypad_devices_panel(&mut state, &board);
        let telemetry = joypad_telemetry();
        state.handle_key(key(KeyCode::Down), &board, &telemetry);
        assert_eq!(state.devices.cursor, 1);
        // The Traffic panel's own state is untouched by Devices interaction.
        assert_eq!(state.traffic.sort, TrafficSort::Rate);
        assert!(state.traffic.filter.is_empty());
        assert_eq!(state.traffic.scroll, 0);
    }

    #[test]
    fn resources_pause_freezes_the_displayed_sample_until_resumed() {
        let mut state = AppState::new();
        let board = board_with(&[(
            "tool-telemetry",
            ParticipantKind::Tool,
            ParticipantState::Ready,
        )]);
        state.sync(&board);
        state.handle_key(key(KeyCode::Enter), &board, &TelemetrySnapshot::default());
        state.handle_key(key(KeyCode::Right), &board, &TelemetrySnapshot::default());
        state.handle_key(key(KeyCode::Right), &board, &TelemetrySnapshot::default());
        assert_eq!(state.panel, Panel::Resources);

        let mut telemetry = TelemetrySnapshot::default();
        let sample = HostSample {
            cpu_pct: 55.0,
            ram_used_bytes: 1,
            ram_total_bytes: 2,
            load_1m: 0.1,
            window_ns: 1,
        };
        telemetry.host = Some(Timestamped {
            value: sample,
            received_at: Instant::now(),
        });
        state.handle_key(key(KeyCode::Char('p')), &board, &telemetry);
        assert!(state.resources.paused);
        assert_eq!(
            state.resources.display_host(None).map(|host| host.cpu_pct),
            Some(55.0),
            "pausing must capture the live sample even though `live` afterward is None"
        );
    }

    #[test]
    fn resources_window_key_cycles_the_range() {
        let mut state = AppState::new();
        let board = board_with(&[(
            "tool-telemetry",
            ParticipantKind::Tool,
            ParticipantState::Ready,
        )]);
        state.sync(&board);
        state.handle_key(key(KeyCode::Enter), &board, &TelemetrySnapshot::default());
        state.handle_key(key(KeyCode::Right), &board, &TelemetrySnapshot::default());
        state.handle_key(key(KeyCode::Right), &board, &TelemetrySnapshot::default());
        assert_eq!(state.resources.range, ResourcesRange::OneMinute);
        state.handle_key(
            key(KeyCode::Char('w')),
            &board,
            &TelemetrySnapshot::default(),
        );
        assert_eq!(state.resources.range, ResourcesRange::TwoMinutes);
    }
}
