//! Interactive TUI state and key handling - deliberately free of any
//! ratatui/crossterm *rendering* concerns so it is unit-testable without a
//! terminal. Only [`AppState::handle_key`]'s input type
//! ([`crossterm::event::KeyEvent`]) ties this to crossterm; construct one
//! directly in a test rather than driving a real terminal.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::display::DisplayAction;
use crate::supervisor::BoardSnapshot;
use crate::tui::groups::{
    Group, GroupSection, bespoke_tab_label, build_groups, suggested_participant,
};

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
    Runtime(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Navigator,
    Detail,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DetailTab {
    Overview,
    Logs,
    Bespoke,
}

/// The full interactive state of the TUI shell, rebuilt against the latest
/// [`BoardSnapshot`] every redraw ([`AppState::sync`]) and mutated by
/// [`AppState::handle_key`].
#[derive(Debug, Clone)]
pub struct AppState {
    pub rows: Vec<NavRow>,
    pub cursor: usize,
    pub view: View,
    pub tab: DetailTab,
    pub focus: Focus,
    pub nav_filter: String,
    pub log_filter: String,
    pub filtering: bool,
    pub log_scroll: usize,
    pub log_follow: bool,
    pub overview_scroll: usize,
    pub show_help: bool,
    /// Becomes `true` the first time the user moves the cursor themselves;
    /// until then, `sync` keeps steering the cursor at the suggested
    /// (failed/degraded, else starting) row so the navigator opens already
    /// pointed at what needs attention.
    user_moved_cursor: bool,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            rows: Vec::new(),
            cursor: 0,
            view: View::Home,
            tab: DetailTab::Overview,
            focus: Focus::Navigator,
            nav_filter: String::new(),
            log_filter: String::new(),
            filtering: false,
            log_scroll: 0,
            log_follow: true,
            overview_scroll: 0,
            show_help: false,
            user_moved_cursor: false,
        }
    }
}

impl AppState {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Recompute the flattened navigator rows against the latest board, and -
    /// until the user has navigated by hand - keep the cursor on the
    /// suggested row (design doc: "if any entity is failed/degraded,
    /// highlight it as the suggested selection; else suggest the first
    /// still-starting entity").
    pub fn sync(&mut self, board: &BoardSnapshot) {
        let filter = self.nav_filter.to_lowercase();
        let sections = build_groups(board, &filter);
        self.rows = flatten(&sections);
        if self.cursor >= self.rows.len() {
            self.cursor = self.rows.len().saturating_sub(1);
        }
        if !matches!(self.rows.get(self.cursor), Some(NavRow::Participant(_))) {
            self.cursor = first_selectable(&self.rows).unwrap_or(0);
        }
        if !self.user_moved_cursor
            && let Some(suggested) = suggested_participant(board)
            && let Some(index) = self
                .rows
                .iter()
                .position(|row| matches!(row, NavRow::Participant(id) if id == &suggested.id))
        {
            self.cursor = index;
        }
    }

    #[must_use]
    pub fn selected_id(&self) -> Option<&str> {
        match self.rows.get(self.cursor) {
            Some(NavRow::Participant(id)) => Some(id.as_str()),
            _ => None,
        }
    }

    /// Handle one key event, returning what the supervisor loop must act on
    /// (`DisplayAction::None` for anything purely internal to the TUI).
    pub fn handle_key(&mut self, key: KeyEvent, board: &BoardSnapshot) -> DisplayAction {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return DisplayAction::Quit;
        }
        if self.filtering {
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
        match self.focus {
            Focus::Navigator => self.handle_navigator_key(key, board),
            Focus::Detail => self.handle_detail_key(key),
        }
    }

    fn handle_filter_key(&mut self, key: KeyEvent) -> DisplayAction {
        let buffer = if self.focus == Focus::Detail && self.tab == DetailTab::Logs {
            &mut self.log_filter
        } else {
            &mut self.nav_filter
        };
        match key.code {
            KeyCode::Enter | KeyCode::Esc => self.filtering = false,
            KeyCode::Backspace => {
                buffer.pop();
            }
            KeyCode::Char(character) => buffer.push(character),
            _ => {}
        }
        DisplayAction::None
    }

    fn handle_navigator_key(&mut self, key: KeyEvent, _board: &BoardSnapshot) -> DisplayAction {
        match key.code {
            KeyCode::Char('q') => return DisplayAction::Quit,
            KeyCode::Char('/') => self.filtering = true,
            KeyCode::Up => self.move_cursor(-1),
            KeyCode::Down => self.move_cursor(1),
            KeyCode::Enter => {
                if let Some(id) = self.selected_id() {
                    self.view = View::Runtime(id.to_string());
                    self.tab = DetailTab::Overview;
                    self.overview_scroll = 0;
                    self.log_scroll = 0;
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

    fn handle_detail_key(&mut self, key: KeyEvent) -> DisplayAction {
        let View::Runtime(id) = &self.view else {
            self.focus = Focus::Navigator;
            return DisplayAction::None;
        };
        let tabs = available_tabs(id);
        match key.code {
            KeyCode::Char('q') => return DisplayAction::Quit,
            KeyCode::Esc => {
                self.view = View::Home;
                self.focus = Focus::Navigator;
            }
            KeyCode::Left => self.tab = cycle_tab(&tabs, self.tab, -1),
            KeyCode::Right => self.tab = cycle_tab(&tabs, self.tab, 1),
            KeyCode::Up => self.scroll(-1),
            KeyCode::Down => self.scroll(1),
            KeyCode::Char('f') if self.tab == DetailTab::Logs => {
                self.log_follow = !self.log_follow;
            }
            KeyCode::Char('/') if self.tab == DetailTab::Logs => self.filtering = true,
            _ => {}
        }
        DisplayAction::None
    }

    fn move_cursor(&mut self, delta: isize) {
        self.user_moved_cursor = true;
        let selectable = selectable_indices(&self.rows);
        if selectable.is_empty() {
            return;
        }
        let current_pos = selectable
            .iter()
            .position(|&index| index == self.cursor)
            .unwrap_or(0);
        let len = selectable.len() as isize;
        let next = (current_pos as isize + delta).rem_euclid(len) as usize;
        self.cursor = selectable[next];
    }

    fn scroll(&mut self, delta: isize) {
        let target = if self.tab == DetailTab::Logs {
            &mut self.log_scroll
        } else {
            &mut self.overview_scroll
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

/// The tabs available for `id`'s detail surface: Overview + Logs always, plus
/// a bespoke third tab for the three hardcoded system tools (design doc).
#[must_use]
pub fn available_tabs(id: &str) -> Vec<DetailTab> {
    let mut tabs = vec![DetailTab::Overview, DetailTab::Logs];
    if bespoke_tab_label(id).is_some() {
        tabs.push(DetailTab::Bespoke);
    }
    tabs
}

fn cycle_tab(tabs: &[DetailTab], current: DetailTab, delta: isize) -> DetailTab {
    let Some(index) = tabs.iter().position(|tab| *tab == current) else {
        return tabs.first().copied().unwrap_or(DetailTab::Overview);
    };
    let len = tabs.len() as isize;
    let next = ((index as isize + delta).rem_euclid(len)) as usize;
    tabs[next]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::participant_kind::ParticipantKind;
    use crate::supervisor::{ParticipantState, ParticipantStatus};
    use crossterm::event::{KeyEventKind, KeyEventState};

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
            state.handle_key(ctrl_c(), &board),
            DisplayAction::Quit
        ));
    }

    #[test]
    fn q_quits_in_the_navigator() {
        let mut state = AppState::new();
        let board = board_with(&[("drive", ParticipantKind::Service, ParticipantState::Ready)]);
        state.sync(&board);
        assert!(matches!(
            state.handle_key(key(KeyCode::Char('q')), &board),
            DisplayAction::Quit
        ));
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
        state.handle_key(key(KeyCode::Down), &board);
        let second = state.selected_id().map(str::to_string);
        assert_ne!(
            first, second,
            "moving down must land on a different participant"
        );
        state.handle_key(key(KeyCode::Down), &board);
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
        state.handle_key(key(KeyCode::Enter), &board);
        assert_eq!(state.view, View::Runtime("drive".to_string()));
        assert_eq!(state.focus, Focus::Detail);
        assert_eq!(state.tab, DetailTab::Overview);
    }

    #[test]
    fn esc_from_detail_returns_home_and_focus_to_navigator() {
        let mut state = AppState::new();
        let board = board_with(&[("drive", ParticipantKind::Service, ParticipantState::Ready)]);
        state.sync(&board);
        state.handle_key(key(KeyCode::Enter), &board);
        state.handle_key(key(KeyCode::Esc), &board);
        assert_eq!(state.view, View::Home);
        assert_eq!(state.focus, Focus::Navigator);
    }

    #[test]
    fn r_restart_emits_a_restart_action_for_the_selected_row() {
        let mut state = AppState::new();
        let board = board_with(&[("drive", ParticipantKind::Service, ParticipantState::Ready)]);
        state.sync(&board);
        let action = state.handle_key(key(KeyCode::Char('r')), &board);
        assert!(matches!(action, DisplayAction::Restart(id) if id == "drive"));
    }

    #[test]
    fn tab_cycling_includes_bespoke_tab_only_for_hardcoded_system_tools() {
        assert_eq!(
            available_tabs("tool-router"),
            vec![DetailTab::Overview, DetailTab::Logs, DetailTab::Bespoke]
        );
        assert_eq!(
            available_tabs("drive"),
            vec![DetailTab::Overview, DetailTab::Logs]
        );
    }

    #[test]
    fn left_right_cycles_tabs_and_wraps() {
        let mut state = AppState::new();
        let board = board_with(&[(
            "tool-router",
            ParticipantKind::Tool,
            ParticipantState::Ready,
        )]);
        state.sync(&board);
        state.handle_key(key(KeyCode::Enter), &board);
        assert_eq!(state.tab, DetailTab::Overview);
        state.handle_key(key(KeyCode::Right), &board);
        assert_eq!(state.tab, DetailTab::Logs);
        state.handle_key(key(KeyCode::Right), &board);
        assert_eq!(state.tab, DetailTab::Bespoke);
        state.handle_key(key(KeyCode::Right), &board);
        assert_eq!(state.tab, DetailTab::Overview, "must wrap back around");
    }

    #[test]
    fn slash_enters_filter_mode_and_typed_characters_accumulate() {
        let mut state = AppState::new();
        let board = board_with(&[("drive", ParticipantKind::Service, ParticipantState::Ready)]);
        state.sync(&board);
        state.handle_key(key(KeyCode::Char('/')), &board);
        assert!(state.filtering);
        state.handle_key(key(KeyCode::Char('d')), &board);
        state.handle_key(key(KeyCode::Char('r')), &board);
        assert_eq!(state.nav_filter, "dr");
        state.handle_key(key(KeyCode::Enter), &board);
        assert!(!state.filtering);
    }

    #[test]
    fn help_toggle_does_not_leak_into_navigation() {
        let mut state = AppState::new();
        let board = board_with(&[("drive", ParticipantKind::Service, ParticipantState::Ready)]);
        state.sync(&board);
        state.handle_key(key(KeyCode::Char('?')), &board);
        assert!(state.show_help);
        // Any other key dismisses help rather than navigating.
        state.handle_key(key(KeyCode::Down), &board);
        assert!(!state.show_help);
    }
}
