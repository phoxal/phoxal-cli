//! Drawing: turns [`AppState`] + a [`BoardSnapshot`] + the [`LogRouter`]
//! scrollback into ratatui widgets. Pure rendering - no state mutation, no
//! I/O - so every function here can be exercised against a
//! [`ratatui::backend::TestBackend`] in tests without a real terminal.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Tabs, Wrap};
use unicode_width::UnicodeWidthStr;

use crate::supervisor::{BoardSnapshot, ParticipantState, ParticipantStatus};
use crate::theme::{Role, Theme, state_symbol};
use crate::tui::color;
use crate::tui::groups::bespoke_tab_label;
use crate::tui::logs::LogRouter;
use crate::tui::state::{AppState, DetailTab, Focus, NavRow, View, available_tabs};

/// Static identity shown on the title bar - resolved once at TUI
/// construction, not re-derived every frame.
#[derive(Debug, Clone)]
pub struct TitleInfo {
    pub robot: String,
    pub channel: String,
    pub mode: &'static str,
}

/// Right-aligned insertion point on the title line for the simulation
/// step/time readout. Empty today - a later slice fills this from the
/// framework's `y2026_9` simulation clock once `simulation run` is active.
/// Kept as its own function (rather than inlined) precisely so that slice
/// has one line to change.
#[must_use]
pub fn simulation_clock_slot() -> String {
    String::new()
}

/// Right-aligned insertion point on the status line for the host CPU/RAM
/// meter. A later slice (Phase 3c, `y2026_9::process`) fills this in;
/// today it renders a plain placeholder rather than nothing, so the layout
/// slot is visibly reserved.
#[must_use]
pub fn host_resource_slot() -> String {
    "cpu n/a".to_string()
}

pub fn draw(
    frame: &mut Frame,
    theme: Theme,
    title: &TitleInfo,
    board: &BoardSnapshot,
    logs: &mut LogRouter,
    state: &AppState,
) {
    let area = frame.area();
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(1),
        ])
        .split(area);

    draw_title_bar(frame, theme, title, rows[0]);
    draw_status_line(frame, theme, board, rows[1]);
    draw_body(frame, theme, board, logs, state, rows[2]);
    draw_footer(frame, theme, state, rows[3]);

    if state.show_help {
        draw_help_overlay(frame, theme, area);
    }
}

/// Build one line with `left` flush to the start and `right` flush to the
/// end, padded with plain spaces in between (or truncated if both together
/// overflow `width`) - the shared "insertion-point" layout for the title
/// and status rows.
fn split_line(width: u16, left: Vec<Span<'static>>, right: Vec<Span<'static>>) -> Line<'static> {
    let width = width as usize;
    let left_width: usize = left.iter().map(|span| span.content.width()).sum();
    let right_width: usize = right.iter().map(|span| span.content.width()).sum();
    let mut spans = left;
    if right_width > 0 && left_width + right_width < width {
        let gap = width - left_width - right_width;
        spans.push(Span::raw(" ".repeat(gap)));
        spans.extend(right);
    }
    Line::from(spans)
}

fn draw_title_bar(frame: &mut Frame, theme: Theme, title: &TitleInfo, area: Rect) {
    let left = vec![
        Span::styled("phoxal", color::fg(theme, Role::Accent)),
        Span::raw(" · "),
        Span::styled(title.robot.clone(), color::fg(theme, Role::TextPrimary)),
        Span::raw(" · "),
        Span::styled(title.channel.clone(), color::fg(theme, Role::TextPrimary)),
        Span::raw(" · "),
        Span::styled(title.mode, color::fg(theme, Role::TextPrimary)),
    ];
    let clock = simulation_clock_slot();
    let right = if clock.is_empty() {
        Vec::new()
    } else {
        vec![Span::styled(clock, color::muted(theme))]
    };
    let line = split_line(area.width, left, right);
    frame.render_widget(
        Paragraph::new(line).style(color::fg(theme, Role::Text)),
        area,
    );
}

fn draw_status_line(frame: &mut Frame, theme: Theme, board: &BoardSnapshot, area: Rect) {
    let total = board.participants.len();
    let connected = board
        .participants
        .values()
        .filter(|status| status.state == ParticipantState::Ready)
        .count();
    let degraded = board
        .participants
        .values()
        .filter(|status| {
            matches!(
                status.state,
                ParticipantState::Degraded | ParticipantState::Failed
            )
        })
        .count();
    let left = vec![Span::styled(
        format!("{connected}/{total} connected · {degraded} degraded"),
        color::fg(theme, Role::TextPrimary),
    )];
    let right = vec![Span::styled(host_resource_slot(), color::muted(theme))];
    let line = split_line(area.width, left, right);
    frame.render_widget(Paragraph::new(line), area);
}

fn draw_body(
    frame: &mut Frame,
    theme: Theme,
    board: &BoardSnapshot,
    logs: &mut LogRouter,
    state: &AppState,
    area: Rect,
) {
    let nav_width = (area.width / 3).clamp(16, 36).min(area.width);
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(nav_width), Constraint::Min(0)])
        .split(area);
    draw_navigator(frame, theme, board, state, columns[0]);
    draw_right_pane(frame, theme, board, logs, state, columns[1]);
}

fn draw_navigator(
    frame: &mut Frame,
    theme: Theme,
    board: &BoardSnapshot,
    state: &AppState,
    area: Rect,
) {
    let block = Block::default()
        .borders(Borders::RIGHT)
        .border_style(color::muted(theme));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let width = inner.width.saturating_sub(1) as usize;
    let items: Vec<ListItem> = state
        .rows
        .iter()
        .map(|row| navigator_row_item(theme, board, row, width))
        .collect();
    let mut list_state = ListState::default();
    list_state.select(Some(state.cursor));
    let list = List::new(items)
        .highlight_style(color::selected(theme, Role::Text))
        .highlight_symbol("▍");
    frame.render_stateful_widget(list, inner, &mut list_state);
}

fn navigator_row_item<'a>(
    theme: Theme,
    board: &BoardSnapshot,
    row: &NavRow,
    width: usize,
) -> ListItem<'a> {
    match row {
        NavRow::Header(group) => ListItem::new(Line::from(Span::styled(
            format!(" {}", group.label()),
            color::muted(theme).add_modifier(Modifier::BOLD),
        ))),
        NavRow::Participant(id) => {
            let role = board
                .participants
                .get(id)
                .map_or(Role::TextPrimary, |status| {
                    crate::theme::state_role(status.state)
                });
            let text = board
                .participants
                .get(id)
                .map_or_else(|| id.clone(), |status| participant_row_text(status, width));
            ListItem::new(Line::from(Span::styled(
                format!(" {text}"),
                color::fg(theme, role),
            )))
        }
    }
}

/// Render one participant row's full text (state symbol/label, id, local
/// marker, restart count) - split out from `navigator_row_item` so it can be
/// unit-tested for unicode-width truncation without a `Frame`.
#[must_use]
pub fn participant_row_text(status: &ParticipantStatus, width: usize) -> String {
    let symbol = state_symbol(status.state);
    let local = if status.local { "*" } else { " " };
    let restarts = if status.restart_count > 0 {
        format!(" ↻{}", status.restart_count)
    } else {
        String::new()
    };
    let text = format!("{symbol} {local}{}{restarts}", status.id);
    truncate_to_width(&text, width)
}

/// Truncate `text` to at most `width` display columns (unicode-width aware,
/// not byte/char-count aware), appending an ellipsis marker when truncated so
/// the cut is visible rather than silent.
#[must_use]
pub fn truncate_to_width(text: &str, width: usize) -> String {
    if text.width() <= width {
        return text.to_string();
    }
    if width == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut used = 0usize;
    for ch in text.chars() {
        let ch_width = UnicodeWidthStr::width(ch.encode_utf8(&mut [0; 4]) as &str);
        if used + ch_width > width.saturating_sub(1) {
            break;
        }
        out.push(ch);
        used += ch_width;
    }
    out.push('…');
    out
}

fn draw_right_pane(
    frame: &mut Frame,
    theme: Theme,
    board: &BoardSnapshot,
    logs: &mut LogRouter,
    state: &AppState,
    area: Rect,
) {
    match &state.view {
        View::Home => draw_overview_home(frame, theme, board, area),
        View::Runtime(id) => draw_runtime_detail(frame, theme, board, logs, state, id, area),
    }
}

fn draw_overview_home(frame: &mut Frame, theme: Theme, board: &BoardSnapshot, area: Rect) {
    let starting: Vec<&str> = board
        .participants
        .values()
        .filter(|status| matches!(status.state, ParticipantState::Starting))
        .map(|status| status.id.as_str())
        .collect();
    let unhealthy: Vec<&str> = board
        .participants
        .values()
        .filter(|status| {
            matches!(
                status.state,
                ParticipantState::Failed | ParticipantState::Degraded
            )
        })
        .map(|status| status.id.as_str())
        .collect();
    let restarted: Vec<&str> = board
        .participants
        .values()
        .filter(|status| status.restart_count > 0)
        .map(|status| status.id.as_str())
        .collect();
    let suggested =
        crate::tui::groups::suggested_participant(board).map(|status| status.id.as_str());

    let operational = unhealthy.is_empty() && starting.is_empty() && !board.participants.is_empty();

    let mut lines = vec![
        Line::from(Span::styled(
            if operational {
                "Operational"
            } else {
                "Not fully operational"
            },
            color::fg(
                theme,
                if operational {
                    Role::Success
                } else {
                    Role::Warn
                },
            ),
        )),
        Line::from(""),
    ];
    lines.push(Line::from(Span::styled(
        format!("Starting: {}", join_or_none(&starting)),
        color::fg(theme, Role::TextPrimary),
    )));
    lines.push(Line::from(Span::styled(
        format!("Unhealthy: {}", join_or_none(&unhealthy)),
        color::fg(theme, Role::TextPrimary),
    )));
    lines.push(Line::from(Span::styled(
        format!("Restarted: {}", join_or_none(&restarted)),
        color::fg(theme, Role::TextPrimary),
    )));
    lines.push(Line::from(""));
    if let Some(id) = suggested {
        lines.push(Line::from(vec![
            Span::styled("Suggested: ", color::fg(theme, Role::Accent)),
            Span::styled(id, color::fg(theme, Role::Text)),
        ]));
    }

    let block = Block::default().title(" Overview ").borders(Borders::NONE);
    frame.render_widget(
        Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn join_or_none(ids: &[&str]) -> String {
    if ids.is_empty() {
        "none".to_string()
    } else {
        ids.join(", ")
    }
}

fn draw_runtime_detail(
    frame: &mut Frame,
    theme: Theme,
    board: &BoardSnapshot,
    logs: &mut LogRouter,
    state: &AppState,
    id: &str,
    area: Rect,
) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(1)])
        .split(area);

    let tabs = available_tabs(id);
    let titles: Vec<Line> = tabs
        .iter()
        .map(|tab| Line::from(tab_label(*tab, id)))
        .collect();
    let selected = tabs.iter().position(|tab| *tab == state.tab).unwrap_or(0);
    let tabs_widget = Tabs::new(titles)
        .select(selected)
        .highlight_style(color::fg(theme, Role::Accent).add_modifier(Modifier::BOLD))
        .style(color::muted(theme));
    frame.render_widget(tabs_widget, rows[0]);

    let Some(status) = board.participants.get(id) else {
        frame.render_widget(
            Paragraph::new(format!("{id} is no longer on the board")).style(color::muted(theme)),
            rows[1],
        );
        return;
    };

    match state.tab {
        DetailTab::Overview => {
            draw_runtime_overview(frame, theme, status, state.overview_scroll, rows[1])
        }
        DetailTab::Logs => draw_runtime_logs(frame, theme, logs, state, id, rows[1]),
        DetailTab::Bespoke => draw_bespoke_placeholder(frame, theme, id, rows[1]),
    }
}

fn tab_label(tab: DetailTab, id: &str) -> String {
    match tab {
        DetailTab::Overview => "Overview".to_string(),
        DetailTab::Logs => "Logs".to_string(),
        DetailTab::Bespoke => bespoke_tab_label(id).unwrap_or("Bespoke").to_string(),
    }
}

fn draw_runtime_overview(
    frame: &mut Frame,
    theme: Theme,
    status: &ParticipantStatus,
    scroll: usize,
    area: Rect,
) {
    let mut lines = vec![
        Line::from(vec![
            Span::styled("state    ", color::muted(theme)),
            Span::raw(theme.participant_state(status.state)),
        ]),
        Line::from(vec![
            Span::styled("kind     ", color::muted(theme)),
            Span::raw(status.kind.label()),
        ]),
        Line::from(vec![
            Span::styled("local    ", color::muted(theme)),
            Span::raw(if status.local { "yes" } else { "no" }),
        ]),
        Line::from(vec![
            Span::styled("restarts ", color::muted(theme)),
            Span::raw(status.restart_count.to_string()),
        ]),
        Line::from(vec![
            Span::styled("cpu      ", color::muted(theme)),
            Span::raw("- (Phase 3c)"),
        ]),
        Line::from(vec![
            Span::styled("ram      ", color::muted(theme)),
            Span::raw("- (Phase 3c)"),
        ]),
        Line::from(vec![
            Span::styled("last error ", color::muted(theme)),
            Span::raw(status.note.clone().unwrap_or_else(|| "-".to_string())),
        ]),
    ];
    if let Some(command) = &status.launch_command {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled("command", color::muted(theme))));
        lines.push(Line::from(command.command_line.clone()));
    }
    let paragraph = Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .scroll((scroll as u16, 0));
    frame.render_widget(paragraph, area);
}

fn draw_runtime_logs(
    frame: &mut Frame,
    theme: Theme,
    logs: &mut LogRouter,
    state: &AppState,
    id: &str,
    area: Rect,
) {
    let filter = state.log_filter.to_lowercase();
    let lines: Vec<Line> = logs
        .lines_for(id)
        .iter()
        .filter(|line| filter.is_empty() || line.text.to_lowercase().contains(&filter))
        .map(|line| {
            let tag = match line.source {
                crate::supervisor::LogSource::Bus => "",
                crate::supervisor::LogSource::Raw => "[raw] ",
            };
            Line::from(Span::styled(
                format!("{tag}{}", line.text),
                color::fg(theme, Role::TextPrimary),
            ))
        })
        .collect();

    let height = area.height as usize;
    let scroll = if state.log_follow {
        lines.len().saturating_sub(height)
    } else {
        state
            .log_scroll
            .min(lines.len().saturating_sub(height.min(lines.len())))
    };
    let paragraph = Paragraph::new(lines).scroll((scroll as u16, 0));
    frame.render_widget(paragraph, area);
}

fn draw_bespoke_placeholder(frame: &mut Frame, theme: Theme, id: &str, area: Rect) {
    let label = bespoke_tab_label(id).unwrap_or("Bespoke");
    frame.render_widget(
        Paragraph::new(format!("{label} - coming soon")).style(color::muted(theme)),
        area,
    );
}

fn draw_footer(frame: &mut Frame, theme: Theme, state: &AppState, area: Rect) {
    let segments: Vec<&str> = footer_segments(state);
    let mut text = segments.join("  ");
    text = truncate_to_width(&text, area.width as usize);
    frame.render_widget(Paragraph::new(text).style(color::muted(theme)), area);
}

fn footer_segments(state: &AppState) -> Vec<&'static str> {
    if state.filtering {
        return vec!["type to filter", "↵/Esc done"];
    }
    match state.focus {
        Focus::Navigator => vec![
            "↑↓ select",
            "↵ inspect",
            "r restart",
            "/ filter",
            "? help",
            "q quit",
        ],
        Focus::Detail if state.tab == DetailTab::Logs => {
            vec![
                "↑↓ scroll",
                "f follow",
                "/ filter",
                "←→ tab",
                "Esc back",
                "? help",
                "q quit",
            ]
        }
        Focus::Detail => vec!["←→ tab", "↑↓ scroll", "Esc back", "? help", "q quit"],
    }
}

fn draw_help_overlay(frame: &mut Frame, theme: Theme, area: Rect) {
    let width = (area.width.saturating_sub(4)).min(60);
    let height = 12.min(area.height.saturating_sub(2));
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    let popup = Rect::new(x, y, width, height);
    frame.render_widget(Clear, popup);
    let lines = vec![
        Line::from("Keys"),
        Line::from(""),
        Line::from("↑↓        move / scroll"),
        Line::from("↵         inspect selected runtime"),
        Line::from("←→        switch tab"),
        Line::from("r         restart selected runtime"),
        Line::from("/         filter"),
        Line::from("f         toggle log follow"),
        Line::from("Esc       back"),
        Line::from("q, Ctrl-C quit"),
        Line::from("?         toggle this help"),
    ];
    let block = Block::default()
        .title(" Help ")
        .borders(Borders::ALL)
        .border_style(color::fg(theme, Role::Accent));
    frame.render_widget(
        Paragraph::new(lines)
            .block(block)
            .alignment(Alignment::Left)
            .style(Style::default()),
        popup,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_line_pads_between_left_and_right() {
        let left = vec![Span::raw("left")];
        let right = vec![Span::raw("right")];
        let line = split_line(20, left, right);
        let rendered: String = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert_eq!(rendered.width(), 20.min(rendered.width()));
        assert!(rendered.starts_with("left"));
        assert!(rendered.ends_with("right"));
    }

    #[test]
    fn split_line_drops_the_right_side_when_it_would_overflow() {
        let left = vec![Span::raw("a very long left side indeed")];
        let right = vec![Span::raw("right")];
        let line = split_line(10, left, right);
        let rendered: String = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert!(!rendered.contains("right"));
    }

    #[test]
    fn truncate_to_width_keeps_short_text_untouched() {
        assert_eq!(truncate_to_width("short", 20), "short");
    }

    #[test]
    fn truncate_to_width_marks_a_cut_with_an_ellipsis() {
        let truncated = truncate_to_width("a rather long participant id here", 10);
        assert!(truncated.width() <= 10);
        assert!(truncated.ends_with('…'));
    }

    #[test]
    fn truncate_to_width_is_unicode_width_aware_not_byte_aware() {
        // Wide (double-width) CJK characters must count as 2 columns each,
        // not 1 byte/char - a naive `chars().take(n)` would fit twice as
        // many as actually display.
        let text = "宽字符宽字符宽字符";
        let truncated = truncate_to_width(text, 6);
        assert!(
            truncated.width() <= 6,
            "{truncated} exceeds 6 display columns"
        );
    }

    #[test]
    fn footer_collapses_to_filter_hint_while_filtering() {
        let mut state = AppState::new();
        state.filtering = true;
        let segments = footer_segments(&state);
        assert!(segments.iter().any(|segment| segment.contains("filter")));
        assert!(!segments.iter().any(|segment| segment.contains("restart")));
    }

    #[test]
    fn participant_row_text_truncates_a_wide_unicode_id_to_the_given_width() {
        let status = ParticipantStatus::new(
            "宽字符宽字符宽字符",
            crate::participant_kind::ParticipantKind::Service,
            ParticipantState::Ready,
        );
        let text = participant_row_text(&status, 8);
        assert!(
            text.width() <= 8,
            "{text} exceeds the requested 8-column budget"
        );
    }

    #[test]
    fn participant_row_text_shows_the_restart_count_only_when_nonzero() {
        let mut status = ParticipantStatus::new(
            "drive",
            crate::participant_kind::ParticipantKind::Service,
            ParticipantState::Ready,
        );
        assert!(!participant_row_text(&status, 40).contains('↻'));
        status.restart_count = 3;
        assert!(participant_row_text(&status, 40).contains("↻3"));
    }

    fn sample_board() -> BoardSnapshot {
        let mut board = BoardSnapshot::default();
        for (id, kind, state) in [
            (
                "tool-router",
                crate::participant_kind::ParticipantKind::Tool,
                ParticipantState::Ready,
            ),
            (
                "drive",
                crate::participant_kind::ParticipantKind::Service,
                ParticipantState::Ready,
            ),
            (
                "left_wheel",
                crate::participant_kind::ParticipantKind::Driver,
                ParticipantState::Failed,
            ),
        ] {
            board
                .participants
                .insert(id.to_string(), ParticipantStatus::new(id, kind, state));
        }
        board
    }

    fn sample_title() -> TitleInfo {
        TitleInfo {
            robot: "rover-01".to_string(),
            channel: "dev".to_string(),
            mode: "run",
        }
    }

    /// A full frame at a very ordinary size must render without panicking,
    /// and every top-level chrome element (title, group headings, footer)
    /// must actually land somewhere in the buffer.
    #[test]
    fn draws_a_full_frame_at_80x24_without_panicking() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        let theme = Theme::new(crate::theme::ColorCapability::None);
        let title = sample_title();
        let board = sample_board();
        let mut logs = LogRouter::new();
        let mut state = AppState::new();
        state.sync(&board);

        terminal
            .draw(|frame| draw(frame, theme, &title, &board, &mut logs, &state))
            .expect("draw must not fail at 80x24");

        let content = buffer_text(terminal.backend().buffer());
        assert!(content.contains("phoxal"), "title bar missing: {content}");
        assert!(
            content.contains("System"),
            "System group heading missing: {content}"
        );
        assert!(content.contains("quit"), "footer hint missing: {content}");
    }

    /// A pathologically narrow terminal (design doc: "a very narrow width")
    /// must degrade gracefully - no panic, no underflow in the width-based
    /// layout math (`nav_width` clamping, `split_line`'s saturating
    /// subtraction, `truncate_to_width`).
    #[test]
    fn draws_without_panicking_at_a_very_narrow_width() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        for (width, height) in [(20, 10), (8, 6), (1, 3)] {
            let backend = TestBackend::new(width, height);
            let mut terminal = Terminal::new(backend).expect("test terminal");
            let theme = Theme::new(crate::theme::ColorCapability::None);
            let title = sample_title();
            let board = sample_board();
            let mut logs = LogRouter::new();
            let mut state = AppState::new();
            state.sync(&board);

            terminal
                .draw(|frame| draw(frame, theme, &title, &board, &mut logs, &state))
                .unwrap_or_else(|error| panic!("draw must not fail at {width}x{height}: {error}"));
        }
    }

    /// A resize mid-session (the operator's terminal window changing size)
    /// must not panic on the next redraw - `ratatui::Terminal::resize`
    /// mirrors what a real `crossterm::event::Event::Resize` drives.
    #[test]
    fn survives_a_resize_event_between_two_redraws() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        let theme = Theme::new(crate::theme::ColorCapability::None);
        let title = sample_title();
        let board = sample_board();
        let mut logs = LogRouter::new();
        let mut state = AppState::new();
        state.sync(&board);

        terminal
            .draw(|frame| draw(frame, theme, &title, &board, &mut logs, &state))
            .expect("first draw must succeed");

        terminal
            .resize(ratatui::layout::Rect::new(0, 0, 40, 12))
            .expect("resize must succeed");

        terminal
            .draw(|frame| draw(frame, theme, &title, &board, &mut logs, &state))
            .expect("draw after resize must succeed");
    }

    /// Every `ColorCapability` tier (including the two non-truecolor
    /// fallbacks) must render a full frame without panicking - the TUI reuses
    /// `theme`'s own degradation (`crate::tui::color`), so this is the
    /// integration proof that the degradation actually reaches every widget.
    #[test]
    fn draws_without_panicking_across_every_color_capability() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        for capability in [
            crate::theme::ColorCapability::TrueColor,
            crate::theme::ColorCapability::Ansi256,
            crate::theme::ColorCapability::Ansi16,
            crate::theme::ColorCapability::None,
        ] {
            let backend = TestBackend::new(80, 24);
            let mut terminal = Terminal::new(backend).expect("test terminal");
            let theme = Theme::new(capability);
            let title = sample_title();
            let board = sample_board();
            let mut logs = LogRouter::new();
            let mut state = AppState::new();
            state.sync(&board);

            terminal
                .draw(|frame| draw(frame, theme, &title, &board, &mut logs, &state))
                .unwrap_or_else(|error| panic!("draw must not fail under {capability:?}: {error}"));
        }
    }

    fn buffer_text(buffer: &ratatui::buffer::Buffer) -> String {
        let mut text = String::new();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                text.push_str(buffer[(x, y)].symbol());
            }
            text.push('\n');
        }
        text
    }
}
