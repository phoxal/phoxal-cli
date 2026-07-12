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

use crate::launch_plan::{SITE_TOOL_JOYPAD, SITE_TOOL_ROUTER, SITE_TOOL_TELEMETRY};
use crate::supervisor::{BoardSnapshot, ClockSample, ParticipantState, ParticipantStatus};
use crate::telemetry::{
    HostSample, JoypadDevicesSample, RouterMetricsSample, TelemetrySnapshot, TopicMetric,
};
use crate::theme::{Role, Theme, state_symbol};
use crate::tui::color;
use crate::tui::groups::bespoke_tab_label;
use crate::tui::logs::{HostPoint, LogRouter};
use crate::tui::state::{AppState, DetailTab, Focus, NavRow, TrafficSort, View, available_tabs};

/// Static identity shown on the title bar - resolved once at TUI
/// construction, not re-derived every frame.
#[derive(Debug, Clone)]
pub struct TitleInfo {
    pub robot: String,
    pub channel: String,
    pub mode: &'static str,
}

/// Right-aligned insertion point on the title line for the simulation
/// step/time readout: `step <n> · <sim>s` while the clock is running, dimmed
/// `step <n> … paused` while it is observed but not advancing (`running ==
/// false` is an OBSERVED state from the clock sample itself, never inferred
/// from feed silence). Empty in `run` mode (no sim clock ever exists there)
/// or before the first sample arrives.
#[must_use]
pub fn simulation_clock_slot(mode: &str, clock: Option<ClockSample>) -> String {
    if mode != "simulation" {
        return String::new();
    }
    let Some(sample) = clock else {
        return String::new();
    };
    if sample.running {
        let seconds = sample.now_ns as f64 / 1_000_000_000.0;
        format!("step {} · {seconds:.1}s", sample.step)
    } else {
        format!("step {} … paused", sample.step)
    }
}

/// Right-aligned insertion point on the status line for the host CPU/RAM
/// meter: `cpu NN% · ram U/T` from the latest `telemetry::Host` sample, or
/// `cpu n/a` when none has arrived yet - `tool-telemetry` may not be in the
/// catalog yet, and that MUST render as a graceful absence, not an error.
#[must_use]
pub fn host_resource_slot(host: Option<HostSample>) -> String {
    let Some(host) = host else {
        return "cpu n/a".to_string();
    };
    format!(
        "cpu {:.0}% · ram {}/{}",
        host.cpu_pct,
        format_gib(host.ram_used_bytes),
        format_gib(host.ram_total_bytes),
    )
}

/// The theme role the host meter should render in - accent/warn/error by the
/// higher of CPU/RAM load (`theme::resource_role`), or muted for the `n/a`
/// case.
#[must_use]
fn host_resource_role(host: Option<HostSample>) -> Role {
    let Some(host) = host else {
        return Role::Muted;
    };
    let cpu_load = f64::from(host.cpu_pct) / 100.0;
    let ram_load = if host.ram_total_bytes > 0 {
        host.ram_used_bytes as f64 / host.ram_total_bytes as f64
    } else {
        0.0
    };
    crate::theme::resource_role(cpu_load.max(ram_load))
}

/// Render `bytes` as whole gibibytes with one decimal (`"3.2G"`) - compact
/// enough that the status line's `cpu NN% · ram U/T` insertion point never
/// crowds out the left side even on an 80-column terminal.
fn format_gib(bytes: u64) -> String {
    format!("{:.1}G", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
}

pub fn draw(
    frame: &mut Frame,
    theme: Theme,
    title: &TitleInfo,
    board: &BoardSnapshot,
    logs: &mut LogRouter,
    state: &AppState,
    telemetry: &TelemetrySnapshot,
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

    draw_title_bar(frame, theme, title, telemetry.clock, rows[0]);
    draw_status_line(frame, theme, board, telemetry.host, rows[1]);
    draw_body(frame, theme, board, logs, state, telemetry, rows[2]);
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

fn draw_title_bar(
    frame: &mut Frame,
    theme: Theme,
    title: &TitleInfo,
    clock: Option<ClockSample>,
    area: Rect,
) {
    let left = vec![
        Span::styled("phoxal", color::fg(theme, Role::Accent)),
        Span::raw(" · "),
        Span::styled(title.robot.clone(), color::fg(theme, Role::TextPrimary)),
        Span::raw(" · "),
        Span::styled(title.channel.clone(), color::fg(theme, Role::TextPrimary)),
        Span::raw(" · "),
        Span::styled(title.mode, color::fg(theme, Role::TextPrimary)),
    ];
    let clock_text = simulation_clock_slot(title.mode, clock);
    let right = if clock_text.is_empty() {
        Vec::new()
    } else {
        vec![Span::styled(clock_text, color::muted(theme))]
    };
    let line = split_line(area.width, left, right);
    frame.render_widget(
        Paragraph::new(line).style(color::fg(theme, Role::Text)),
        area,
    );
}

fn draw_status_line(
    frame: &mut Frame,
    theme: Theme,
    board: &BoardSnapshot,
    host: Option<HostSample>,
    area: Rect,
) {
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
    let right = vec![Span::styled(
        host_resource_slot(host),
        color::fg(theme, host_resource_role(host)),
    )];
    let line = split_line(area.width, left, right);
    frame.render_widget(Paragraph::new(line), area);
}

fn draw_body(
    frame: &mut Frame,
    theme: Theme,
    board: &BoardSnapshot,
    logs: &mut LogRouter,
    state: &AppState,
    telemetry: &TelemetrySnapshot,
    area: Rect,
) {
    let nav_width = (area.width / 3).clamp(16, 36).min(area.width);
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(nav_width), Constraint::Min(0)])
        .split(area);
    draw_navigator(frame, theme, board, state, columns[0]);
    draw_right_pane(frame, theme, board, logs, state, telemetry, columns[1]);
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
    telemetry: &TelemetrySnapshot,
    area: Rect,
) {
    match &state.view {
        View::Home => draw_overview_home(frame, theme, board, area),
        View::Runtime(id) => {
            draw_runtime_detail(frame, theme, board, logs, state, telemetry, id, area);
        }
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

#[allow(clippy::too_many_arguments)]
fn draw_runtime_detail(
    frame: &mut Frame,
    theme: Theme,
    board: &BoardSnapshot,
    logs: &mut LogRouter,
    state: &AppState,
    telemetry: &TelemetrySnapshot,
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
        DetailTab::Overview => draw_runtime_overview(
            frame,
            theme,
            status,
            telemetry.process_by_participant.get(id).copied(),
            state.overview_scroll,
            rows[1],
        ),
        DetailTab::Logs => draw_runtime_logs(frame, theme, logs, state, id, rows[1]),
        DetailTab::Bespoke => {
            draw_bespoke_tab(frame, theme, id, state, logs, telemetry, rows[1]);
        }
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
    process: Option<crate::telemetry::ProcessSample>,
    scroll: usize,
    area: Rect,
) {
    let cpu_text = process.map_or_else(
        || "-".to_string(),
        |sample| format!("{:.0}%", sample.cpu_pct),
    );
    let ram_text = process.map_or_else(
        || "-".to_string(),
        |sample| format!("{}M", sample.rss_bytes / (1024 * 1024)),
    );
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
            Span::raw(cpu_text),
        ]),
        Line::from(vec![
            Span::styled("ram      ", color::muted(theme)),
            Span::raw(ram_text),
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

/// Dispatch a bespoke third tab to its tool-specific renderer - the
/// hardcoded id -> tab mapping (design doc: "hardcode the tool -> tab
/// mapping; no generic abstraction"), matching `groups::bespoke_tab_label`.
fn draw_bespoke_tab(
    frame: &mut Frame,
    theme: Theme,
    id: &str,
    state: &AppState,
    logs: &LogRouter,
    telemetry: &TelemetrySnapshot,
    area: Rect,
) {
    if id == SITE_TOOL_ROUTER {
        draw_traffic_tab(
            frame,
            theme,
            state,
            logs.latest_traffic_sample_for(id),
            area,
        );
    } else if id == SITE_TOOL_JOYPAD {
        draw_joypad_tab(frame, theme, state, telemetry.joypad.as_ref(), area);
    } else if id == SITE_TOOL_TELEMETRY {
        draw_resources_tab(
            frame,
            theme,
            telemetry.host,
            logs.telemetry_series_for(id),
            area,
        );
    } else {
        draw_bespoke_placeholder(frame, theme, id, area);
    }
}

fn draw_bespoke_placeholder(frame: &mut Frame, theme: Theme, id: &str, area: Rect) {
    let label = bespoke_tab_label(id).unwrap_or("Bespoke");
    frame.render_widget(
        Paragraph::new(format!("{label} - coming soon")).style(color::muted(theme)),
        area,
    );
}

/// A topic's ingress rate has dropped to effectively zero over the router's
/// measurement window - rendered as `stale` rather than `live` in the
/// Traffic table's STATUS column.
const STALE_RATE_THRESHOLD_HZ: f32 = 0.01;

#[must_use]
fn topic_is_stale(topic: &TopicMetric) -> bool {
    topic.ingress_rate_hz < STALE_RATE_THRESHOLD_HZ
}

fn sort_traffic_rows(topics: &mut [&TopicMetric], sort: TrafficSort) {
    match sort {
        TrafficSort::Rate => {
            topics.sort_by(|left, right| right.ingress_rate_hz.total_cmp(&left.ingress_rate_hz));
        }
        TrafficSort::Topic => topics.sort_by(|left, right| left.topic.cmp(&right.topic)),
        TrafficSort::Producer => {
            topics.sort_by(|left, right| left.from_participant.cmp(&right.from_participant));
        }
        TrafficSort::Status => topics.sort_by(|left, right| {
            topic_is_stale(right)
                .cmp(&topic_is_stale(left))
                .then_with(|| right.ingress_rate_hz.total_cmp(&left.ingress_rate_hz))
        }),
    }
}

/// The router Traffic tab: a sortable (`s`), filterable (`/`) table of every
/// topic the router has measured ingress for - TOPIC, PRODUCER, RATE, COUNT,
/// and a live/stale STATUS derived from the measured rate itself (design
/// doc: "stale if a topic's rate dropped to ~0 over the window"). Header
/// line carries the router's own aggregate `throughput_msg_s`.
fn draw_traffic_tab(
    frame: &mut Frame,
    theme: Theme,
    state: &AppState,
    sample: Option<&RouterMetricsSample>,
    area: Rect,
) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(1)])
        .split(area);

    let Some(sample) = sample else {
        frame.render_widget(
            Paragraph::new("no router metrics observed yet - cpu n/a until tool-router publishes")
                .style(color::muted(theme)),
            area,
        );
        return;
    };

    frame.render_widget(
        Paragraph::new(format!(
            "throughput {:.1} msg/s · {} topic(s) · sort:{}",
            sample.throughput_msg_s,
            sample.topics.len(),
            state.traffic_sort.label(),
        ))
        .style(color::muted(theme)),
        rows[0],
    );

    let filter = state.traffic_filter.to_lowercase();
    let mut topics: Vec<&TopicMetric> = sample
        .topics
        .iter()
        .filter(|topic| {
            filter.is_empty()
                || topic.topic.to_lowercase().contains(&filter)
                || topic.from_participant.to_lowercase().contains(&filter)
        })
        .collect();
    sort_traffic_rows(&mut topics, state.traffic_sort);

    let width = rows[1].width as usize;
    let mut lines = vec![Line::from(Span::styled(
        traffic_header_row(width),
        color::muted(theme).add_modifier(Modifier::BOLD),
    ))];
    lines.extend(
        topics
            .iter()
            .map(|topic| traffic_row_line(theme, topic, width)),
    );

    let height = rows[1].height as usize;
    let scroll = state
        .traffic_scroll
        .min(lines.len().saturating_sub(height.min(lines.len())));
    let paragraph = Paragraph::new(lines).scroll((scroll as u16, 0));
    frame.render_widget(paragraph, rows[1]);
}

fn traffic_header_row(width: usize) -> String {
    truncate_to_width(
        &format!(
            "{:<28} {:<16} {:>8} {:>9} {:>6}",
            "TOPIC", "PRODUCER", "RATE", "COUNT", "STATUS"
        ),
        width,
    )
}

fn traffic_row_line(theme: Theme, topic: &TopicMetric, width: usize) -> Line<'static> {
    let stale = topic_is_stale(topic);
    let producer = if topic.from_participant.is_empty() {
        "-"
    } else {
        topic.from_participant.as_str()
    };
    let text = format!(
        "{:<28} {:<16} {:>6.1}Hz {:>9} {:>6}",
        truncate_to_width(&topic.topic, 28),
        truncate_to_width(producer, 16),
        topic.ingress_rate_hz,
        topic.count,
        if stale { "stale" } else { "live" },
    );
    let role = if stale { Role::Muted } else { Role::Success };
    Line::from(Span::styled(
        truncate_to_width(&text, width),
        color::fg(theme, role),
    ))
}

/// The joypad Devices tab: pure list navigation over
/// `telemetry::JoypadDevicesSample::available` (`↑↓` moves
/// `AppState::joypad_cursor`, a purely local UI cursor), with the
/// AUTHORITATIVE `selected` device (from the tool's own last ack, never a
/// local guess) marked separately from the cursor highlight.
fn draw_joypad_tab(
    frame: &mut Frame,
    theme: Theme,
    state: &AppState,
    sample: Option<&JoypadDevicesSample>,
    area: Rect,
) {
    let Some(sample) = sample else {
        frame.render_widget(
            Paragraph::new("no joypad devices observed yet").style(color::muted(theme)),
            area,
        );
        return;
    };

    let mut lines = Vec::new();
    if sample.available.is_empty() {
        lines.push(Line::from(Span::styled(
            "no gamepads detected",
            color::muted(theme),
        )));
    }
    for (index, device) in sample.available.iter().enumerate() {
        let dot = if device.connected { "●" } else { "○" };
        let marker = if sample.selected.as_deref() == Some(device.id.as_str()) {
            "✓"
        } else {
            " "
        };
        let cursor = if index == state.joypad_cursor {
            "▍"
        } else {
            " "
        };
        let role = if index == state.joypad_cursor {
            Role::Accent
        } else if device.connected {
            Role::TextPrimary
        } else {
            Role::Muted
        };
        lines.push(Line::from(Span::styled(
            format!("{cursor}{marker} {dot} {}", device.name),
            color::fg(theme, role),
        )));
    }
    if let Some(error) = &sample.last_error {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            format!("error: {error}"),
            color::fg(theme, Role::Error),
        )));
    }
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

/// One 9-level sparkline block per value in `[0.0, 1.0]` (clamped), oldest
/// first - a compact rolling-history readout that still degrades to a plain
/// glyph run under `ColorCapability::None` (the fill level IS the signal, not
/// the color, matching `theme::resource_meter`'s convention).
const SPARK_LEVELS: [char; 9] = [' ', '▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

fn sparkline_line(theme: Theme, values: impl Iterator<Item = f64>) -> Line<'static> {
    let text: String = values
        .map(|value| {
            let clamped = value.clamp(0.0, 1.0);
            let index = (clamped * (SPARK_LEVELS.len() - 1) as f64).round() as usize;
            SPARK_LEVELS[index.min(SPARK_LEVELS.len() - 1)]
        })
        .collect();
    if text.is_empty() {
        Line::from(Span::styled("no samples yet", color::muted(theme)))
    } else {
        Line::from(Span::styled(text, color::fg(theme, Role::Accent)))
    }
}

/// The `tool-telemetry` Resources tab: the latest `Host` sample's headline
/// numbers plus a rolling cpu%/ram% sparkline history from
/// `LogRouter::telemetry_series_for` (design doc Part 3c/4d).
fn draw_resources_tab(
    frame: &mut Frame,
    theme: Theme,
    host: Option<HostSample>,
    series: &[HostPoint],
    area: Rect,
) {
    let Some(host) = host else {
        frame.render_widget(
            Paragraph::new("cpu n/a - tool-telemetry has not published a sample yet")
                .style(color::muted(theme)),
            area,
        );
        return;
    };
    let ram_pct = if host.ram_total_bytes > 0 {
        host.ram_used_bytes as f64 / host.ram_total_bytes as f64 * 100.0
    } else {
        0.0
    };
    let mut lines = vec![
        Line::from(vec![
            Span::styled("cpu    ", color::muted(theme)),
            Span::raw(format!("{:.0}%", host.cpu_pct)),
        ]),
        Line::from(vec![
            Span::styled("ram    ", color::muted(theme)),
            Span::raw(format!(
                "{} / {} ({ram_pct:.0}%)",
                format_gib(host.ram_used_bytes),
                format_gib(host.ram_total_bytes),
            )),
        ]),
        Line::from(vec![
            Span::styled("load 1m ", color::muted(theme)),
            Span::raw(format!("{:.2}", host.load_1m)),
        ]),
        Line::from(""),
        Line::from(Span::styled("cpu history", color::muted(theme))),
        sparkline_line(
            theme,
            series.iter().map(|point| f64::from(point.cpu_pct) / 100.0),
        ),
        Line::from(Span::styled("ram history", color::muted(theme))),
        sparkline_line(
            theme,
            series.iter().map(|point| {
                if point.ram_total_bytes > 0 {
                    point.ram_used_bytes as f64 / point.ram_total_bytes as f64
                } else {
                    0.0
                }
            }),
        ),
    ];
    lines.push(Line::from(""));
    let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
    frame.render_widget(paragraph, area);
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
        Focus::Detail if state.on_router_traffic_tab() => vec![
            "↑↓ scroll",
            "s sort",
            "/ filter",
            "←→ tab",
            "Esc back",
            "? help",
            "q quit",
        ],
        Focus::Detail if state.on_joypad_devices_tab() => vec![
            "↑↓ select",
            "↵ connect",
            "r rescan",
            "←→ tab",
            "Esc back",
            "? help",
            "q quit",
        ],
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
    fn simulation_clock_slot_is_empty_outside_simulation_mode() {
        let sample = ClockSample {
            now_ns: 5_000_000_000,
            step: 42,
            running: true,
        };
        assert_eq!(simulation_clock_slot("run", Some(sample)), "");
    }

    #[test]
    fn simulation_clock_slot_is_empty_before_the_first_sample() {
        assert_eq!(simulation_clock_slot("simulation", None), "");
    }

    #[test]
    fn simulation_clock_slot_shows_step_and_seconds_while_running() {
        let sample = ClockSample {
            now_ns: 5_500_000_000,
            step: 42,
            running: true,
        };
        let text = simulation_clock_slot("simulation", Some(sample));
        assert!(text.contains("step 42"), "{text}");
        assert!(text.contains("5.5s"), "{text}");
        assert!(!text.contains("paused"), "{text}");
    }

    #[test]
    fn simulation_clock_slot_shows_paused_when_the_observed_clock_is_not_running() {
        let sample = ClockSample {
            now_ns: 5_000_000_000,
            step: 7,
            running: false,
        };
        let text = simulation_clock_slot("simulation", Some(sample));
        assert!(text.contains("step 7"), "{text}");
        assert!(text.contains("paused"), "{text}");
    }

    #[test]
    fn host_resource_slot_shows_n_a_when_no_sample_has_arrived() {
        assert_eq!(host_resource_slot(None), "cpu n/a");
        assert_eq!(host_resource_role(None), Role::Muted);
    }

    #[test]
    fn host_resource_slot_renders_cpu_and_ram_once_a_sample_arrives() {
        let host = HostSample {
            cpu_pct: 42.0,
            ram_used_bytes: 4 * 1024 * 1024 * 1024,
            ram_total_bytes: 16 * 1024 * 1024 * 1024,
            load_1m: 0.9,
            window_ns: 1_000_000_000,
        };
        let text = host_resource_slot(Some(host));
        assert!(text.contains("cpu 42%"), "{text}");
        assert!(text.contains("ram"), "{text}");
        assert!(text.contains("4.0G"), "{text}");
        assert!(text.contains("16.0G"), "{text}");
    }

    fn topic(name: &str, producer: &str, rate: f32, count: u64) -> TopicMetric {
        TopicMetric {
            topic: name.to_string(),
            from_participant: producer.to_string(),
            ingress_rate_hz: rate,
            count,
        }
    }

    #[test]
    fn topic_is_stale_below_the_threshold_only() {
        assert!(topic_is_stale(&topic("a", "p", 0.0, 10)));
        assert!(!topic_is_stale(&topic("a", "p", 5.0, 10)));
    }

    #[test]
    fn sort_traffic_rows_by_rate_is_highest_first() {
        let a = topic("a", "p", 1.0, 1);
        let b = topic("b", "p", 9.0, 1);
        let c = topic("c", "p", 3.0, 1);
        let mut rows = vec![&a, &b, &c];
        sort_traffic_rows(&mut rows, TrafficSort::Rate);
        assert_eq!(
            rows.iter()
                .map(|row| row.topic.as_str())
                .collect::<Vec<_>>(),
            vec!["b", "c", "a"]
        );
    }

    #[test]
    fn sort_traffic_rows_by_topic_is_alphabetic() {
        let a = topic("zeta", "p", 1.0, 1);
        let b = topic("alpha", "p", 1.0, 1);
        let mut rows = vec![&a, &b];
        sort_traffic_rows(&mut rows, TrafficSort::Topic);
        assert_eq!(
            rows.iter()
                .map(|row| row.topic.as_str())
                .collect::<Vec<_>>(),
            vec!["alpha", "zeta"]
        );
    }

    #[test]
    fn sort_traffic_rows_by_status_surfaces_stale_topics_first() {
        let live = topic("live", "p", 5.0, 1);
        let stale = topic("stale", "p", 0.0, 1);
        let mut rows = vec![&live, &stale];
        sort_traffic_rows(&mut rows, TrafficSort::Status);
        assert_eq!(rows[0].topic, "stale");
    }

    /// Traffic table renders `stale`/`live` per row and honors the filter
    /// buffer - the acceptance criterion for the Traffic tab's sort/filter
    /// model, exercised through the real `draw_traffic_tab` against a
    /// `TestBackend` rather than re-deriving the row text by hand.
    #[test]
    fn traffic_tab_renders_filtered_rows_with_stale_status() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let sample = RouterMetricsSample {
            topics: vec![
                topic("y2026_1/drive/target", "mission", 12.0, 500),
                topic("y2026_1/battery/state", "battery", 0.0, 10),
            ],
            throughput_msg_s: 12.0,
            window_ns: 1_000_000_000,
        };
        let mut state = AppState::new();
        state.traffic_filter = "battery".to_string();

        let backend = TestBackend::new(80, 10);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        let theme = Theme::new(crate::theme::ColorCapability::None);
        terminal
            .draw(|frame| draw_traffic_tab(frame, theme, &state, Some(&sample), frame.area()))
            .expect("draw must not fail");
        let content = buffer_text(terminal.backend().buffer());
        assert!(content.contains("battery"), "{content}");
        assert!(content.contains("stale"), "{content}");
        assert!(!content.contains("drive/target"), "{content}");
    }

    /// The joypad Devices tab marks the AUTHORITATIVE `selected` device from
    /// the sample - never a local guess - independent of the cursor
    /// position, which is exactly the "selection updates from the tool's own
    /// next Devices publish" acceptance criterion.
    #[test]
    fn joypad_tab_marks_the_authoritative_selection_not_the_cursor() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let sample = JoypadDevicesSample {
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
            selected: Some("pad-b".to_string()),
            last_error: None,
        };
        let mut state = AppState::new();
        state.joypad_cursor = 0; // cursor on pad-a, selection is pad-b

        let backend = TestBackend::new(60, 10);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        let theme = Theme::new(crate::theme::ColorCapability::None);
        terminal
            .draw(|frame| draw_joypad_tab(frame, theme, &state, Some(&sample), frame.area()))
            .expect("draw must not fail");
        let content = buffer_text(terminal.backend().buffer());
        let pad_a_line = content.lines().find(|line| line.contains("Pad A")).unwrap();
        let pad_b_line = content.lines().find(|line| line.contains("Pad B")).unwrap();
        assert!(!pad_a_line.contains('✓'), "{pad_a_line}");
        assert!(pad_b_line.contains('✓'), "{pad_b_line}");
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
            .draw(|frame| {
                draw(
                    frame,
                    theme,
                    &title,
                    &board,
                    &mut logs,
                    &state,
                    &TelemetrySnapshot::default(),
                )
            })
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
                .draw(|frame| {
                    draw(
                        frame,
                        theme,
                        &title,
                        &board,
                        &mut logs,
                        &state,
                        &TelemetrySnapshot::default(),
                    )
                })
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
            .draw(|frame| {
                draw(
                    frame,
                    theme,
                    &title,
                    &board,
                    &mut logs,
                    &state,
                    &TelemetrySnapshot::default(),
                )
            })
            .expect("first draw must succeed");

        terminal
            .resize(ratatui::layout::Rect::new(0, 0, 40, 12))
            .expect("resize must succeed");

        terminal
            .draw(|frame| {
                draw(
                    frame,
                    theme,
                    &title,
                    &board,
                    &mut logs,
                    &state,
                    &TelemetrySnapshot::default(),
                )
            })
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
                .draw(|frame| {
                    draw(
                        frame,
                        theme,
                        &title,
                        &board,
                        &mut logs,
                        &state,
                        &TelemetrySnapshot::default(),
                    )
                })
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
