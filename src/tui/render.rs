//! Rendering for the fixed Overview, Runtimes, Logs, Bus, and Input pages.

use std::collections::BTreeMap;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use phoxal_cli_core::session::human;
#[cfg(test)]
use phoxal_cli_ui::ColorCapability;
use phoxal_cli_ui::ratatui as color;
use phoxal_cli_ui::{Role, Theme};
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Borders, Clear, HighlightSpacing, List, ListItem, ListState, Paragraph,
    Sparkline, Wrap,
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

#[cfg(test)]
use crate::launch_plan::SITE_TOOL_JOYPAD;
use crate::session::controller::SessionMode;
use crate::session::event::PhaseOutcome;
use crate::stores::log_store::sanitize_terminal_text;
use crate::stores::telemetry_store::{DEFAULT_FRESHNESS_TTL, Timestamped};
use crate::supervisor::{ClockSample, LogSeverity, ParticipantState, ParticipantStatus};
#[cfg(test)]
use crate::telemetry::JoypadDevicesSample;
use crate::telemetry::{HostSample, JoypadDeviceStatus, TelemetrySnapshot, TopicMetric};
use crate::tui::startup::{PhaseRow, StartupState};
#[cfg(test)]
use crate::tui::state::LogSourceFilter;
use crate::tui::state::{AppState, BusSort, CaseInsensitiveNeedle, NavigationLevel, Page};
use crate::tui::view_model::{RuntimeGroup, SessionViewModel};

#[derive(Debug, Clone)]
pub struct TitleInfo {
    pub robot: String,
    pub namespace: String,
    pub channel: String,
    pub manifest: String,
    pub mode: SessionMode,
    pub bus_endpoint: String,
    pub started_at: SystemTime,
    pub started_instant: Instant,
}

fn state_role(state: ParticipantState) -> Role {
    match state {
        ParticipantState::Ready => Role::Success,
        ParticipantState::Starting | ParticipantState::Restarting => Role::Steel,
        ParticipantState::Degraded => Role::Warn,
        ParticipantState::Failed => Role::Error,
        ParticipantState::Stopped => Role::Muted,
    }
}

fn state_symbol(state: ParticipantState) -> &'static str {
    match state {
        ParticipantState::Starting => "…",
        ParticipantState::Ready => "✓",
        ParticipantState::Degraded => "!",
        ParticipantState::Failed => "✗",
        ParticipantState::Restarting => "↻",
        ParticipantState::Stopped => "■",
    }
}

pub struct StartupView<'a> {
    pub title: &'a TitleInfo,
    pub startup: &'a StartupState,
    pub state: &'a AppState,
    pub telemetry: &'a TelemetrySnapshot,
    pub now: Instant,
}

#[must_use]
pub fn simulation_clock_slot(
    mode: SessionMode,
    clock: Option<Timestamped<ClockSample>>,
    now: Instant,
) -> String {
    match mode {
        SessionMode::Run => "logical real time".to_string(),
        SessionMode::Simulation => clock.map_or_else(
            || "logical n/a".to_string(),
            |sample| {
                let paused = if sample.is_stale(now, DEFAULT_FRESHNESS_TTL) {
                    " · paused"
                } else {
                    ""
                };
                format!(
                    "step {} · {}{paused}",
                    sample.value.step,
                    human::duration(Duration::from_nanos(sample.value.now_ns))
                )
            },
        ),
    }
}

#[must_use]
pub fn host_resource_slot(host: Option<&Timestamped<HostSample>>, now: Instant) -> String {
    let Some(host) = host else {
        return "cpu n/a · ram n/a".to_string();
    };
    let stale = if host.is_stale(now, DEFAULT_FRESHNESS_TTL) {
        " · stale"
    } else {
        ""
    };
    format!(
        "cpu {:.0}% · ram {}/{}{stale}",
        host.value.cpu_pct,
        human::bytes_compact(host.value.ram_used_bytes),
        human::bytes_compact(host.value.ram_total_bytes),
    )
}

pub fn draw(
    frame: &mut Frame,
    theme: Theme,
    title: &TitleInfo,
    state: &mut AppState,
    model: &SessionViewModel<'_>,
) {
    let area = frame.area();
    frame.render_widget(Clear, area);
    if draw_too_small(frame, theme, state, area) {
        return;
    }

    let expanded_header = area.height >= 21 && area.width >= 80;
    let header_height = if expanded_header { 7 } else { 4 };
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(header_height),
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(1),
        ])
        .split(area);
    draw_header(
        frame,
        theme,
        title,
        model.telemetry,
        model.now,
        rows[0],
        expanded_header,
    );
    draw_tabs(frame, theme, state, rows[1]);
    match state.page {
        Page::Overview => draw_overview(frame, theme, state, model, rows[2]),
        Page::Runtimes => draw_runtimes(frame, theme, state, model, rows[2]),
        Page::Logs => draw_logs(frame, theme, state, model, rows[2]),
        Page::Bus => draw_bus(frame, theme, state, model, rows[2]),
        Page::Input => draw_input(frame, theme, state, model, rows[2]),
    }
    draw_footer(frame, theme, state, rows[3]);
    if state.show_help {
        draw_help(frame, theme, area);
    }
    if state.show_info {
        draw_session_info(frame, theme, title, model, area);
    }
}

pub fn draw_startup(frame: &mut Frame, theme: Theme, view: &StartupView<'_>) {
    let area = frame.area();
    frame.render_widget(Clear, area);
    if draw_too_small(frame, theme, view.state, area) {
        return;
    }
    let expanded_header = area.height >= 21 && area.width >= 80;
    let header_height = if expanded_header { 7 } else { 4 };
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(header_height),
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(1),
        ])
        .split(area);
    draw_header(
        frame,
        theme,
        view.title,
        view.telemetry,
        view.now,
        rows[0],
        expanded_header,
    );
    draw_tabs(frame, theme, view.state, rows[1]);
    let mut lines = Vec::new();
    lines.push(Line::from(format!(
        "manifest   {}",
        sanitize_terminal_text(&view.title.manifest)
    )));
    lines.push(Line::from(""));
    lines.push(Line::from(format!(
        "session    {}",
        view.startup.session_state.label()
    )));
    if let Some(phase) = &view.startup.phase {
        lines.extend(startup_phase_lines(phase));
    }
    frame.render_widget(
        Paragraph::new(lines)
            .block(shell_block(theme, "Starting session · preparation"))
            .wrap(Wrap { trim: true }),
        centered(rows[2], 76, 74),
    );
    draw_footer(frame, theme, view.state, rows[3]);
}

fn draw_too_small(frame: &mut Frame, theme: Theme, state: &AppState, area: Rect) -> bool {
    if area.width >= 44 && area.height >= 18 {
        return false;
    }
    let message = Paragraph::new(vec![
        Line::from("Phoxal session"),
        Line::from("Terminal too small"),
        Line::from("Resize to at least 44 x 18"),
        Line::from(format!(
            "1 Overview  2 Runtimes  3 Logs  4 Bus  5 Input  [{}]",
            state.page.label()
        )),
    ])
    .block(shell_block(theme, "Session"))
    .wrap(Wrap { trim: true });
    frame.render_widget(message, area);
    true
}

fn startup_phase_lines(phase: &PhaseRow) -> Vec<Line<'static>> {
    let status = match &phase.outcome {
        None => "in progress".to_string(),
        Some((PhaseOutcome::Succeeded, elapsed)) => {
            format!("done in {}", human::duration(*elapsed))
        }
        Some((PhaseOutcome::Skipped, _)) => "skipped".to_string(),
        Some((PhaseOutcome::Failed { error }, _)) => {
            format!("failed: {}", sanitize_and_ellipsize(error, 52))
        }
    };
    let label = sanitize_and_ellipsize(&phase.label, 28);
    let summary = format!("phase      {label} · {status}");
    let mut lines = vec![Line::from(sanitize_and_ellipsize(&summary, 96))];
    if let Some(progress) = &phase.progress {
        let detail = progress
            .detail
            .as_deref()
            .map(|detail| sanitize_and_ellipsize(detail, 48));
        let progress = format!(
            "progress   {}/{}{}",
            progress.completed,
            progress.total,
            detail.map_or_else(String::new, |detail| format!(" · {detail}"))
        );
        lines.push(Line::from(sanitize_and_ellipsize(&progress, 96)));
    }
    lines
}

fn draw_header(
    frame: &mut Frame,
    theme: Theme,
    title: &TitleInfo,
    telemetry: &TelemetrySnapshot,
    now: Instant,
    area: Rect,
    expanded: bool,
) {
    if !expanded {
        let inner_width = area.width.saturating_sub(2);
        let lines = vec![
            Line::from(vec![
                Span::styled(" ◇ p h o x a l ", color::fg(theme, Role::Accent)),
                Span::raw(format!(
                    "  {} · {}",
                    sanitize_terminal_text(&title.robot),
                    sanitize_terminal_text(&title.namespace)
                )),
            ]),
            Line::from(compact_header_status(title, telemetry, now, inner_width)),
        ];
        frame.render_widget(
            Paragraph::new(lines).block(shell_block(
                theme,
                &format!("phoxal-cli {}", env!("CARGO_PKG_VERSION")),
            )),
            area,
        );
        return;
    }

    let sections = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(36),
            Constraint::Percentage(36),
            Constraint::Percentage(28),
        ])
        .split(area);
    frame.render_widget(
        Paragraph::new(header_identity_lines(theme, title, sections[0].width)).block(shell_block(
            theme,
            &format!("phoxal-cli {}", env!("CARGO_PKG_VERSION")),
        )),
        sections[0],
    );
    frame.render_widget(
        Paragraph::new(header_host_lines(
            telemetry.host.as_ref(),
            now,
            sections[1].width,
        ))
        .block(shell_block(theme, "Host")),
        sections[1],
    );
    frame.render_widget(
        Paragraph::new(header_clock_lines(
            title.mode,
            telemetry.clock,
            title.started_instant,
            now,
        ))
        .block(shell_block(
            theme,
            if title.mode == SessionMode::Simulation {
                "Simulation"
            } else {
                "Session time"
            },
        )),
        sections[2],
    );
}

fn compact_header_status(
    title: &TitleInfo,
    telemetry: &TelemetrySnapshot,
    now: Instant,
    inner_width: u16,
) -> String {
    if inner_width >= 62 {
        return format!(
            " {} · channel {} · {} · {}",
            title.mode,
            sanitize_terminal_text(&title.channel),
            host_resource_slot(telemetry.host.as_ref(), now),
            simulation_clock_slot(title.mode, telemetry.clock, now)
        );
    }
    let cpu = telemetry.host.as_ref().map_or_else(
        || "cpu n/a".to_string(),
        |host| format!("cpu {:.0}%", host.value.cpu_pct),
    );
    let clock = match title.mode {
        SessionMode::Simulation => telemetry.clock.map_or_else(
            || "step n/a".to_string(),
            |sample| {
                let paused = if sample.is_stale(now, DEFAULT_FRESHNESS_TTL) {
                    " paused"
                } else {
                    ""
                };
                format!("step {}{paused}", sample.value.step)
            },
        ),
        SessionMode::Run => format!(
            "uptime {}",
            human::duration(title.started_instant.elapsed())
        ),
    };
    format!(" {} · {cpu} · {clock}", title.mode)
}

fn header_identity_lines(theme: Theme, title: &TitleInfo, width: u16) -> Vec<Line<'static>> {
    let robot = sanitize_terminal_text(&title.robot);
    let namespace = sanitize_terminal_text(&title.namespace);
    let channel = sanitize_terminal_text(&title.channel);
    let mut lines = vec![Line::from(Span::styled(
        "◇  p h o x a l",
        color::fg(theme, Role::Accent).add_modifier(Modifier::BOLD),
    ))];
    if width >= 36 {
        lines.extend([
            Line::from(format!("robot      {robot}")),
            Line::from(format!("namespace  {namespace} · channel {channel}")),
            Line::from(format!("session    {}", title.mode)),
        ]);
    } else {
        lines.extend([
            Line::from(format!("robot      {robot}")),
            Line::from(format!("namespace  {namespace}")),
            Line::from(format!("channel    {channel}")),
            Line::from(format!("session    {}", title.mode)),
        ]);
    }
    lines
}

fn header_host_lines(
    host: Option<&Timestamped<HostSample>>,
    now: Instant,
    width: u16,
) -> Vec<Line<'static>> {
    let row = |label: &str, value: String| Line::from(format!("{label:<12}{value}"));
    let Some(host) = host else {
        return vec![
            row("CPU", "n/a".to_string()),
            row("RAM", "n/a".to_string()),
            row("DISK (root)", "n/a".to_string()),
            row("load", "n/a".to_string()),
            row("state", "waiting for telemetry".to_string()),
        ];
    };
    let disk = host
        .value
        .disks
        .iter()
        .find(|disk| disk.mount_point == "/")
        .map_or_else(
            || "n/a".to_string(),
            |disk| {
                format!(
                    "{}/{}",
                    human::bytes_compact(disk.used_bytes),
                    human::bytes_compact(disk.total_bytes)
                )
            },
        );
    vec![
        row("CPU", format!("{:.1}%", host.value.cpu_pct)),
        row(
            "RAM",
            format!(
                "{}/{}",
                human::bytes_compact(host.value.ram_used_bytes),
                human::bytes_compact(host.value.ram_total_bytes)
            ),
        ),
        row("DISK (root)", disk),
        row(
            "load",
            if width < 32 {
                format!(
                    "{:.1}/{:.1}/{:.1}",
                    host.value.load_1m, host.value.load_5m, host.value.load_15m
                )
            } else {
                format!(
                    "{:.2} / {:.2} / {:.2}",
                    host.value.load_1m, host.value.load_5m, host.value.load_15m
                )
            },
        ),
        row(
            "state",
            if host.is_stale(now, DEFAULT_FRESHNESS_TTL) {
                "stale".to_string()
            } else if host.value.disks_truncated > 0 {
                format!("live · +{} disks omitted", host.value.disks_truncated)
            } else {
                "live".to_string()
            },
        ),
    ]
}

fn header_clock_lines(
    mode: SessionMode,
    clock: Option<Timestamped<ClockSample>>,
    started_instant: Instant,
    now: Instant,
) -> Vec<Line<'static>> {
    match mode {
        SessionMode::Simulation => clock.map_or_else(
            || {
                vec![
                    Line::from("step    n/a"),
                    Line::from("time    n/a"),
                    Line::from("state   waiting"),
                ]
            },
            |clock| {
                let state = if clock.is_stale(now, DEFAULT_FRESHNESS_TTL) {
                    "paused"
                } else {
                    "live"
                };
                vec![
                    Line::from(format!("step    {}", clock.value.step)),
                    Line::from(format!(
                        "time    {}",
                        human::duration(Duration::from_nanos(clock.value.now_ns))
                    )),
                    Line::from(format!("state   {state}")),
                ]
            },
        ),
        SessionMode::Run => {
            let elapsed = started_instant.elapsed();
            vec![
                Line::from("clock   realtime"),
                Line::from(format!("uptime  {}", human::duration(elapsed))),
                Line::from("state   live"),
            ]
        }
    }
}

fn draw_tabs(frame: &mut Frame, theme: Theme, state: &AppState, area: Rect) {
    let mut spans = Vec::new();
    let padding = if area.width >= 60 { 11 } else { 7 };
    for page in Page::ALL {
        let label = if state.page == page && state.navigation == NavigationLevel::Page {
            format!("[{}]", page.label())
        } else {
            page.label().to_string()
        };
        let style = if state.navigation == NavigationLevel::Tabs && state.tab_cursor == page.index()
        {
            color::candidate(theme, Role::Accent)
        } else if state.page == page {
            if state.navigation == NavigationLevel::Page {
                color::selected(theme, Role::Accent).add_modifier(Modifier::BOLD)
            } else {
                color::fg(theme, Role::Accent).add_modifier(Modifier::BOLD)
            }
        } else {
            color::muted(theme)
        };
        spans.push(Span::styled(format!(" {label:<padding$}"), style));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_overview(
    frame: &mut Frame,
    theme: Theme,
    state: &AppState,
    model: &SessionViewModel<'_>,
    area: Rect,
) {
    let attention = model.needs_attention_for_mode(state.simulation);
    let summary = model.summary_for_mode(state.simulation);
    let mut lines = vec![Line::from(format!(
        "ready {}  degraded {}  failed {}  starting {}  restarts {}",
        summary.ready, summary.degraded, summary.failed, summary.starting, summary.restarts
    ))];
    lines.push(Line::from(""));
    if attention.is_empty() {
        lines.push(Line::from(Span::styled(
            "✓ Nothing needs attention",
            color::fg(theme, Role::Success),
        )));
    } else {
        let available_rows = usize::from(area.height.saturating_sub(4));
        let shown = if attention.len() > available_rows {
            available_rows.saturating_sub(1)
        } else {
            attention.len()
        };
        for status in attention.iter().take(shown) {
            lines.push(runtime_attention_line(theme, status, model));
        }
        let omitted = attention.len().saturating_sub(shown);
        if omitted > 0 {
            lines.push(Line::from(Span::styled(
                format!("… +{omitted} more need attention"),
                color::muted(theme),
            )));
        }
    }
    frame.render_widget(
        Paragraph::new(lines).block(shell_block(theme, "Runtime summary · Needs attention")),
        area,
    );
}

fn runtime_attention_line(
    theme: Theme,
    status: &ParticipantStatus,
    model: &SessionViewModel<'_>,
) -> Line<'static> {
    let restarts = model
        .runtime
        .observation(&status.id)
        .map_or(status.restart_count, |observation| {
            observation.displayed_restarts()
        });
    let note = sanitize_and_ellipsize(status.note.as_deref().unwrap_or(""), 40);
    let id = sanitize_and_fit_cell(&status.id, 18);
    Line::from(vec![
        Span::styled(
            format!("{} {id}", state_symbol(status.state)),
            color::fg(theme, state_role(status.state)),
        ),
        Span::raw(format!(
            " {:<10} restarts {restarts} {}",
            status.state.label(),
            note
        )),
    ])
}

fn draw_runtimes(
    frame: &mut Frame,
    theme: Theme,
    state: &AppState,
    model: &SessionViewModel<'_>,
    area: Rect,
) {
    if state.runtime_detail_id.is_some() {
        draw_runtime_detail(frame, theme, state, model, area);
        return;
    }
    let counts = RuntimeGroup::ALL.map(|group| {
        if group == RuntimeGroup::Drivers && state.simulation {
            0
        } else {
            model.runtimes_in_mode(group, state.simulation).len()
        }
    });
    let heights = runtime_section_heights(counts, area.height);
    let mut section_y = area.y;
    for (index, group) in RuntimeGroup::ALL.into_iter().enumerate() {
        let empty = match group {
            RuntimeGroup::UserServices => "No user runtimes",
            RuntimeGroup::FrameworkServices => "No framework runtimes",
            RuntimeGroup::Drivers if state.simulation => "Not loaded in simulation",
            RuntimeGroup::Drivers => "No drivers loaded",
        };
        let section = Rect::new(area.x, section_y, area.width, heights[index]).intersection(area);
        section_y = section_y.saturating_add(heights[index]);
        draw_runtime_group(frame, theme, state, model, group, empty, section);
    }
}

fn runtime_section_heights(counts: [usize; 3], total_height: u16) -> [u16; 3] {
    // `draw_too_small` admits pages only at 18+ terminal rows. After the
    // persistent header, tabs, and footer, the runtime area is therefore at
    // least 12 rows: exactly three four-row boxes at this load-bearing floor.
    let mut heights = [4_u16; 3];
    let targets = counts.map(|count| {
        u16::try_from(count.max(1))
            .unwrap_or(u16::MAX)
            .saturating_add(3)
    });
    let mut remaining = total_height.saturating_sub(heights.iter().sum());
    let mut surplus_index = 0;
    while remaining > 0 {
        let target_index = (0..3)
            .max_by_key(|index| targets[*index].saturating_sub(heights[*index]))
            .filter(|index| targets[*index] > heights[*index])
            .unwrap_or_else(|| {
                let index = surplus_index % 3;
                surplus_index += 1;
                index
            });
        heights[target_index] = heights[target_index].saturating_add(1);
        remaining -= 1;
    }
    heights
}

fn draw_runtime_group(
    frame: &mut Frame,
    theme: Theme,
    state: &AppState,
    model: &SessionViewModel<'_>,
    group: RuntimeGroup,
    empty: &str,
    area: Rect,
) {
    let runtimes = if group == RuntimeGroup::Drivers && state.simulation {
        Vec::new()
    } else {
        model.runtimes_in_mode(group, state.simulation)
    };
    let visible_rows = usize::from(area.height.saturating_sub(3)).max(1);
    let selected_local = runtimes
        .iter()
        .enumerate()
        .find(|(_, (global_index, _))| *global_index == state.runtime_cursor)
        .map(|(local_index, _)| local_index);
    let title = if runtimes.len() > visible_rows {
        let start = selected_local
            .unwrap_or_default()
            .saturating_add(1)
            .saturating_sub(visible_rows);
        let end = start.saturating_add(visible_rows).min(runtimes.len());
        format!(
            "{} · {}-{} of {}",
            group.label(),
            start + 1,
            end,
            runtimes.len()
        )
    } else {
        format!("{} · {}", group.label(), runtimes.len())
    };
    let block = shell_block(theme, &title);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(0)])
        .split(inner);
    let columns = runtime_columns(inner.width);
    frame.render_widget(
        Paragraph::new(Line::styled(
            format!(
                "    {:<id_width$} {:<state_width$} {:<heartbeat_width$} {:>restart_width$}",
                "ID",
                "STATE",
                if columns.compact { "SEEN" } else { "HEARTBEAT" },
                if columns.compact { "RST" } else { "RESTARTS" },
                id_width = columns.id,
                state_width = columns.state,
                heartbeat_width = columns.heartbeat,
                restart_width = columns.restarts,
            ),
            color::muted(theme),
        )),
        rows[0],
    );
    if runtimes.is_empty() {
        frame.render_widget(Paragraph::new(format!("  {empty}")), rows[1]);
        return;
    }
    let items = runtimes
        .iter()
        .map(|(_, status)| ListItem::new(runtime_row(status, model, columns)))
        .collect::<Vec<_>>();
    let mut list_state = ListState::default();
    list_state.select(selected_local);
    frame.render_stateful_widget(
        List::new(items)
            .highlight_style(color::candidate(theme, Role::Accent))
            .highlight_symbol("› ")
            .highlight_spacing(HighlightSpacing::Always),
        rows[1],
        &mut list_state,
    );
}

#[derive(Clone, Copy)]
struct RuntimeColumns {
    id: usize,
    state: usize,
    heartbeat: usize,
    restarts: usize,
    compact: bool,
}

fn runtime_columns(inner_width: u16) -> RuntimeColumns {
    if inner_width >= 68 {
        return RuntimeColumns {
            id: 26,
            state: 11,
            heartbeat: 16,
            restarts: 8,
            compact: false,
        };
    }
    let state = 7;
    let heartbeat = 8;
    let restarts = 5;
    let fixed = 4 + 3 + state + heartbeat + restarts;
    RuntimeColumns {
        id: usize::from(inner_width).saturating_sub(fixed).max(6),
        state,
        heartbeat,
        restarts,
        compact: true,
    }
}

fn runtime_row(
    status: &ParticipantStatus,
    model: &SessionViewModel<'_>,
    columns: RuntimeColumns,
) -> String {
    let observation = model.runtime.observation(&status.id);
    let restarts = observation.map_or(status.restart_count, |observation| {
        observation.displayed_restarts()
    });
    let heartbeat = observation
        .and_then(|observation| observation.last_seen_age(model.now))
        .map_or_else(
            || "n/a".to_string(),
            |age| format!("{} ago", human::duration(age)),
        );
    let id = sanitize_and_fit_cell(&status.id, columns.id);
    let state = fit_cell(status.state.label(), columns.state);
    let heartbeat = fit_cell(&heartbeat, columns.heartbeat);
    format!(
        "{} {id} {state} {heartbeat} {restarts:>restart_width$}",
        state_symbol(status.state),
        restart_width = columns.restarts,
    )
}

fn draw_runtime_detail(
    frame: &mut Frame,
    theme: Theme,
    state: &AppState,
    model: &SessionViewModel<'_>,
    area: Rect,
) {
    let Some(detail_id) = state.runtime_detail_id.as_deref() else {
        return;
    };
    let Some(status) = model.runtimes.iter().find(|status| status.id == detail_id) else {
        frame.render_widget(
            Paragraph::new("Runtime no longer present · Esc back")
                .block(shell_block(theme, "Runtime · unavailable")),
            area,
        );
        return;
    };
    let metadata = model.runtime.metadata(&status.id);
    let observation = model.runtime.observation(&status.id);
    let artifact = metadata
        .and_then(|metadata| metadata.artifact_ref.as_deref())
        .unwrap_or("n/a");
    let artifact = sanitize_and_ellipsize(
        artifact,
        usize::from(area.width / 2).saturating_sub(18).max(1),
    );
    let artifact_size = status
        .artifact_size_bytes
        .map_or_else(|| "n/a".to_string(), human::bytes_compact);
    let pid = status
        .pid
        .map_or_else(|| "n/a".to_string(), |pid| pid.to_string());
    let ownership = metadata
        .map(|metadata| format!("{:?}", metadata.ownership))
        .unwrap_or_else(|| "n/a".to_string());
    let ready_after = model
        .runtime
        .time_to_ready(&status.id)
        .map_or_else(|| "n/a".to_string(), human::duration);
    let uptime = observation.map_or_else(
        || "n/a".to_string(),
        |observation| human::duration(observation.uptime(model.now)),
    );
    let last_seen = observation
        .and_then(|observation| observation.last_seen_age(model.now))
        .map_or_else(
            || "n/a".to_string(),
            |age| format!("{} ago", human::duration(age)),
        );
    let restarts = observation.map_or(status.restart_count, |observation| {
        observation.displayed_restarts()
    });
    let identity = vec![
        Line::from(format!("type          {}", status.kind.label())),
        Line::from(format!(
            "source        {}",
            if status.local { "local" } else { "catalog" }
        )),
        Line::from(format!("artifact      {artifact}")),
        Line::from(format!("artifact size {artifact_size}")),
        Line::from(format!("PID           {pid}")),
    ];
    let lifecycle = vec![
        Line::from(format!("state         {}", status.state.label())),
        Line::from(format!("ownership     {ownership}")),
        Line::from(format!("ready after   {ready_after}")),
        Line::from(format!("uptime        {uptime}")),
        Line::from(format!("heartbeat     {last_seen}")),
        Line::from(format!("restarts      {restarts}")),
    ];
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(8), Constraint::Min(5)])
        .split(area);
    let top = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(52), Constraint::Percentage(48)])
        .split(vertical[0]);
    let title_id = sanitize_and_ellipsize(
        &status.id,
        usize::from(top[0].width).saturating_sub(23).max(1),
    );
    frame.render_widget(
        Paragraph::new(identity)
            .block(shell_block(
                theme,
                &format!("Runtime · {title_id} · Identity"),
            ))
            .wrap(Wrap { trim: false }),
        top[0],
    );
    frame.render_widget(
        Paragraph::new(lifecycle)
            .block(shell_block(theme, "Lifecycle · Esc back"))
            .wrap(Wrap { trim: false }),
        top[1],
    );
    let io = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(vertical[1]);
    draw_contracts(
        frame,
        theme,
        "Inputs",
        metadata.map(|metadata| metadata.input_contracts.as_slice()),
        io[0],
    );
    draw_contracts(
        frame,
        theme,
        "Outputs",
        metadata.map(|metadata| metadata.output_contracts.as_slice()),
        io[1],
    );
}

fn draw_contracts(
    frame: &mut Frame,
    theme: Theme,
    title: &str,
    contracts: Option<&[String]>,
    area: Rect,
) {
    let lines = contracts
        .filter(|contracts| !contracts.is_empty())
        .map_or_else(
            || vec![Line::from("None declared")],
            |contracts| {
                contracts
                    .iter()
                    .map(|contract| {
                        Line::from(format!(
                            "• {}",
                            sanitize_and_ellipsize(
                                contract,
                                usize::from(area.width).saturating_sub(4).max(1)
                            )
                        ))
                    })
                    .collect()
            },
        );
    frame.render_widget(
        Paragraph::new(lines)
            .block(shell_block(theme, title))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn draw_logs(
    frame: &mut Frame,
    theme: Theme,
    state: &mut AppState,
    model: &SessionViewModel<'_>,
    area: Rect,
) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(1)])
        .split(area);
    let editing = state
        .editing_label()
        .map_or(String::new(), |label| format!(" · editing {label}"));
    let controls = [
        format!("Source: {}", state.log_source_filter.label()),
        format!("Participant: {}", empty_as_all(&state.log_runtime_filter)),
        format!("Severity: {}", state.log_severity.label()),
        format!("Contains: {}", empty_as_all(&state.log_text_filter)),
        format!(
            "Follow: {}",
            if state.log_follow { "Live" } else { "Paused" }
        ),
    ];
    let control_line = if area.width < 90 {
        let index = state.log_filter_cursor.min(controls.len() - 1);
        let style = if state.navigation == NavigationLevel::Page {
            color::candidate(theme, Role::Accent)
        } else {
            color::muted(theme)
        };
        Line::styled(format!(" {}/5 {} ", index + 1, controls[index]), style)
    } else {
        Line::from(
            controls
                .into_iter()
                .enumerate()
                .flat_map(|(index, label)| {
                    let style = if state.navigation == NavigationLevel::Page
                        && state.log_filter_cursor == index
                    {
                        color::candidate(theme, Role::Accent)
                    } else {
                        color::muted(theme)
                    };
                    [Span::styled(format!(" {label} "), style), Span::raw("  ")]
                })
                .collect::<Vec<_>>(),
        )
    };
    let action_help = if area.width < 60 {
        format!("←→ filter · Enter change · ↑↓ logs{editing}")
    } else {
        format!("←→ filter · Enter change · ↑↓ logs · Space follow · End latest{editing}")
    };
    frame.render_widget(
        Paragraph::new(vec![
            control_line,
            Line::styled(action_help, color::muted(theme)),
        ]),
        rows[0],
    );
    let runtime_filter = CaseInsensitiveNeedle::new(&state.log_runtime_filter);
    let text_filter = CaseInsensitiveNeedle::new(&state.log_text_filter);
    let filtered = model
        .logs
        .lines()
        .filter(|line| state.log_line_matches(line, model, &runtime_filter, &text_filter))
        .filter(|line| {
            state.log_follow
                || state
                    .log_pause_anchor
                    .is_none_or(|anchor| line.received_at <= anchor)
        })
        .collect::<Vec<_>>();
    let height = usize::from(rows[1].height.saturating_sub(2));
    state.log_scroll = bounded_window_start(state.log_scroll, filtered.len(), height);
    let offset = state.log_scroll;
    let end = filtered.len().saturating_sub(offset);
    let start = end.saturating_sub(height);
    let lines = filtered[start..end]
        .iter()
        .map(|line| {
            let participant_width = if rows[1].width >= 60 { 18 } else { 12 };
            let participant = sanitize_and_fit_cell(&line.participant, participant_width);
            ListItem::new(format!(
                "{:>5} {participant} {}",
                severity_label(line.severity),
                line.text
            ))
        })
        .collect::<Vec<_>>();
    let body = if lines.is_empty() {
        List::new(vec![ListItem::new("No logs match the selected filters")])
    } else {
        List::new(lines)
    };
    frame.render_widget(body.block(shell_block(theme, "Logs")), rows[1]);
}

fn draw_bus(
    frame: &mut Frame,
    theme: Theme,
    state: &mut AppState,
    model: &SessionViewModel<'_>,
    area: Rect,
) {
    let summary_height = (area.height / 3).clamp(5, 15);
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Length(summary_height),
            Constraint::Min(1),
        ])
        .split(area);
    let sample = model.telemetry.router.as_ref();
    let freshness = sample.map_or_else(
        || "n/a".to_string(),
        |sample| {
            let age = model.now.saturating_duration_since(sample.received_at);
            if sample.is_stale(model.now, DEFAULT_FRESHNESS_TTL) {
                format!("stale · {} ago", human::duration(age))
            } else {
                format!("{} ago", human::duration(age))
            }
        },
    );
    let filter = CaseInsensitiveNeedle::new(&state.bus_filter);
    let reveal_internal = state.bus_show_internal;
    let controls = [
        format!("Filter: {}", empty_as_all(&state.bus_filter)),
        format!("Sort: {}", state.bus_sort.label()),
        format!(
            "Internal topics: {}",
            if reveal_internal { "Shown" } else { "Hidden" }
        ),
    ];
    let control_line = if area.width < 80 {
        let index = state.bus_control_cursor.min(controls.len() - 1);
        let style = if state.navigation == NavigationLevel::Page {
            color::candidate(theme, Role::Accent)
        } else {
            color::muted(theme)
        };
        Line::styled(
            format!(
                " {}/3 {} · router {} ",
                index + 1,
                controls[index],
                freshness
            ),
            style,
        )
    } else {
        Line::from(
            controls
                .into_iter()
                .enumerate()
                .flat_map(|(index, label)| {
                    let style = if state.navigation == NavigationLevel::Page
                        && state.bus_control_cursor == index
                    {
                        color::candidate(theme, Role::Accent)
                    } else {
                        color::muted(theme)
                    };
                    [Span::styled(format!(" {label} "), style), Span::raw("  ")]
                })
                .chain([Span::styled(
                    format!("Router freshness: {freshness}"),
                    color::muted(theme),
                )])
                .collect::<Vec<_>>(),
        )
    };
    frame.render_widget(
        Paragraph::new(vec![
            control_line,
            Line::styled(
                "←→ choose control · Enter edit/change · ↑↓ scroll topics",
                color::muted(theme),
            ),
        ]),
        rows[0],
    );
    let Some(sample) = sample else {
        frame.render_widget(
            Paragraph::new(vec![
                Line::from("Router telemetry has not arrived yet."),
                Line::from("Waiting for tool-bus on v2/router/metrics…"),
            ])
            .block(shell_block(theme, "Router status")),
            rows[1],
        );
        frame.render_widget(
            Paragraph::new("No topic traffic observed")
                .block(shell_block(theme, "Topics · Producer · Rate · Count")),
            rows[2],
        );
        return;
    };
    let mut topics = sample
        .value
        .topics
        .iter()
        .filter(|metric| state.bus_metric_matches(metric, model, &filter))
        .collect::<Vec<_>>();
    sort_topics(&mut topics, state.bus_sort);
    let summaries = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
        .split(rows[1]);
    let throughput_history = &model.telemetry.router_throughput_history;
    let spark_width = usize::from(summaries[0].width.saturating_sub(2));
    let visible_history_points = history_tail(throughput_history, spark_width);
    let history = visible_history_points
        .iter()
        .map(|point| (point.value.max(0.0) * 10.0).round() as u64)
        .collect::<Vec<_>>();
    let max = history.iter().copied().max().unwrap_or(1).max(1);
    let total_title = if sample.is_stale(model.now, DEFAULT_FRESHNESS_TTL) {
        format!(
            "Last known · {:.1} messages/s · stale · {}",
            sample.value.throughput_msg_s,
            history_span(visible_history_points),
        )
    } else {
        format!(
            "Total · {:.1} messages/s · {}",
            sample.value.throughput_msg_s,
            history_span(visible_history_points),
        )
    };
    frame.render_widget(
        Sparkline::default()
            .block(shell_block(theme, &total_title))
            .data(&history)
            .max(max)
            .style(color::fg(theme, Role::Accent)),
        summaries[0],
    );

    let mut producers = BTreeMap::<String, (f32, u64)>::new();
    // Keep this panel on the router's full detailed sample so its rates align
    // with the adjacent total as closely as the bounded wire table permits.
    // The overflow aggregate is not a producer and may double-count names
    // already present in detailed rows, so it is intentionally excluded.
    for metric in sample
        .value
        .topics
        .iter()
        .filter(|metric| !metric.aggregate_overflow)
    {
        let participant = if metric.from_participant.is_empty() {
            "unknown"
        } else {
            metric.from_participant.as_str()
        };
        let entry = producers.entry(participant.to_string()).or_default();
        entry.0 += metric.ingress_rate_hz;
        entry.1 = entry.1.saturating_add(metric.count);
    }
    let mut producers = producers.into_iter().collect::<Vec<_>>();
    producers.sort_by(|left, right| {
        right
            .1
            .0
            .total_cmp(&left.1.0)
            .then_with(|| left.0.cmp(&right.0))
    });
    let producer_inner_width = usize::from(summaries[1].width.saturating_sub(2));
    let producer_rate_width = if producer_inner_width >= 42 { 10 } else { 6 };
    let producer_count_width = if producer_inner_width >= 42 { 12 } else { 6 };
    let producer_name_width = producer_inner_width
        .saturating_sub(producer_rate_width + producer_count_width + 2)
        .max(1);
    let mut producer_lines = vec![Line::styled(
        format!(
            "{:<name_width$} {:>rate_width$} {:>count_width$}",
            fit_cell("PRODUCER", producer_name_width),
            ellipsize("MSG/S", producer_rate_width),
            ellipsize("COUNT", producer_count_width),
            name_width = producer_name_width,
            rate_width = producer_rate_width,
            count_width = producer_count_width,
        ),
        color::muted(theme),
    )];
    let has_overflow = sample
        .value
        .topics
        .iter()
        .any(|metric| metric.aggregate_overflow);
    if has_overflow {
        producer_lines.push(Line::styled(
            "Overflow excluded; total still includes it",
            color::muted(theme),
        ));
    }
    let available_rows = usize::from(summaries[1].height.saturating_sub(3))
        .saturating_sub(usize::from(has_overflow));
    let truncated = producers.len() > available_rows;
    let producer_limit = if truncated {
        available_rows.saturating_sub(1)
    } else {
        available_rows
    };
    producer_lines.extend(producers.iter().take(producer_limit).map(
        |(producer, (rate, count))| {
            let producer = sanitize_and_fit_cell(producer, producer_name_width);
            let rate = format!("{rate:.1}");
            Line::from(format!(
                "{producer} {rate:>producer_rate_width$} {count:>producer_count_width$}"
            ))
        },
    ));
    if truncated && available_rows > 0 {
        producer_lines.push(Line::styled(
            format!("… +{} more", producers.len().saturating_sub(producer_limit)),
            color::muted(theme),
        ));
    }
    if producers.is_empty() {
        producer_lines.push(Line::from("No producers observed"));
    }
    frame.render_widget(
        Paragraph::new(producer_lines).block(shell_block(
            theme,
            &format!(
                "All producers · {}/{} shown · internals included",
                producer_limit.min(producers.len()),
                producers.len()
            ),
        )),
        summaries[1],
    );

    let topic_inner_width = usize::from(rows[2].width.saturating_sub(2));
    let topic_rate_width = 9;
    let topic_count_width = if topic_inner_width >= 70 { 9 } else { 6 };
    let topic_text_width = topic_inner_width
        .saturating_sub(topic_rate_width + topic_count_width + 3)
        .max(2);
    let topic_producer_width = (topic_text_width / 3).clamp(8, 20).min(topic_text_width);
    let topic_name_width = topic_text_width.saturating_sub(topic_producer_width).max(1);
    let height = usize::from(rows[2].height.saturating_sub(3));
    state.bus_scroll = bounded_window_start(state.bus_scroll, topics.len(), height);
    let start = state.bus_scroll;
    let mut lines = vec![Line::from(format!(
        "{:<topic_width$} {:<producer_width$} {:>rate_width$} {:>count_width$}",
        fit_cell("TOPIC", topic_name_width),
        fit_cell("PRODUCER", topic_producer_width),
        "RATE",
        "COUNT",
        topic_width = topic_name_width,
        producer_width = topic_producer_width,
        rate_width = topic_rate_width,
        count_width = topic_count_width,
    ))];
    lines.extend(topics.iter().skip(start).take(height).map(|metric| {
        let topic = sanitize_and_fit_cell(&metric.topic, topic_name_width);
        let producer = if metric.aggregate_overflow {
            fit_cell("aggregate", topic_producer_width)
        } else if metric.from_participant.is_empty() {
            fit_cell("unknown", topic_producer_width)
        } else {
            sanitize_and_fit_cell(&metric.from_participant, topic_producer_width)
        };
        let rate = format!("{:.1} Hz", metric.ingress_rate_hz);
        Line::from(format!(
            "{topic} {producer} {rate:>topic_rate_width$} {:>topic_count_width$}",
            metric.count
        ))
    }));
    let shown = topics.len().saturating_sub(start).min(height);
    let visible_end = start.saturating_add(shown);
    let truncation = if sample.value.topics_truncated == 0 {
        String::new()
    } else {
        format!(" · +{} omitted", sample.value.topics_truncated)
    };
    let topic_title = format!(
        "Topics · {}/{} visible · {}-{} of {}{truncation}",
        topics.len(),
        sample.value.topics.len(),
        if shown == 0 { 0 } else { start + 1 },
        visible_end,
        topics.len(),
    );
    frame.render_widget(
        Paragraph::new(lines).block(shell_block(theme, &topic_title)),
        rows[2],
    );
}

fn bounded_window_start(offset: usize, item_count: usize, window_height: usize) -> usize {
    offset.min(item_count.saturating_sub(window_height))
}

fn history_tail<T>(history: &[T], width: usize) -> &[T] {
    &history[history.len().saturating_sub(width)..]
}

fn history_span(history: &[Timestamped<f32>]) -> String {
    match (history.first(), history.last()) {
        (None, _) => "waiting".to_string(),
        (Some(_), Some(_)) if history.len() == 1 => "1 sample".to_string(),
        (Some(first), Some(last)) => format!(
            "last {}",
            human::duration(
                last.received_at
                    .saturating_duration_since(first.received_at)
            )
        ),
        _ => "waiting".to_string(),
    }
}

fn sort_topics(topics: &mut [&TopicMetric], sort: BusSort) {
    match sort {
        BusSort::Rate => topics.sort_by(|left, right| {
            right
                .ingress_rate_hz
                .total_cmp(&left.ingress_rate_hz)
                .then_with(|| left.topic.cmp(&right.topic))
        }),
        BusSort::Topic => topics.sort_by(|left, right| left.topic.cmp(&right.topic)),
        BusSort::Producer => topics.sort_by(|left, right| {
            left.from_participant
                .cmp(&right.from_participant)
                .then_with(|| left.topic.cmp(&right.topic))
        }),
    }
}

fn draw_input(
    frame: &mut Frame,
    theme: Theme,
    state: &AppState,
    model: &SessionViewModel<'_>,
    area: Rect,
) {
    let sections = if area.width >= 84 {
        Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(72), Constraint::Percentage(28)])
            .split(area)
    } else {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(0), Constraint::Length(6)])
            .split(area)
    };
    let joypad = model.telemetry.joypad.as_ref();
    let joypad_is_stale =
        joypad.is_some_and(|sample| sample.is_stale(model.now, DEFAULT_FRESHNESS_TTL));
    let live_joypad = joypad.filter(|sample| !sample.is_stale(model.now, DEFAULT_FRESHNESS_TTL));
    let selected_id = live_joypad.and_then(|joypad| joypad.value.selected.as_deref());
    let (linear, angular, motion_updated) = model.telemetry.motion.as_ref().map_or_else(
        || ("n/a".to_string(), "n/a".to_string(), "n/a".to_string()),
        |motion| {
            (
                format!("{:.3} m/s", motion.value.final_target.linear_x_mps),
                format!("{:.3} rad/s", motion.value.final_target.angular_z_radps),
                format!(
                    "{} ago{}",
                    human::duration(model.now.saturating_duration_since(motion.received_at)),
                    if motion.is_stale(model.now, DEFAULT_FRESHNESS_TTL) {
                        " · stale"
                    } else {
                        ""
                    }
                ),
            )
        },
    );
    let overview = vec![
        Line::from("Command"),
        Line::from(format!("linear  {linear}")),
        Line::from(format!("angular {angular}")),
        Line::from(format!("Motion update {motion_updated}")),
    ];
    let devices = live_joypad
        .map(|joypad| joypad.value.available.as_slice())
        .unwrap_or_default();
    let device_width = sections[0].width.saturating_sub(2);
    let items = devices
        .iter()
        .map(|device| {
            let selected = selected_id.is_some_and(|selected| selected == device.id);
            let item = ListItem::new(device_row(device, selected, device_width));
            if selected {
                item.style(color::fg(theme, Role::Accent).add_modifier(Modifier::BOLD))
            } else {
                item
            }
        })
        .collect::<Vec<_>>();
    let items = if items.is_empty() {
        let empty_state = if joypad_is_stale {
            "Controller state stale · waiting for live tool".to_string()
        } else if let Some(reason) =
            live_joypad.and_then(|sample| sample.value.unavailable_reason.as_deref())
        {
            sanitize_and_fit_cell(
                &format!("Input unavailable · {reason}"),
                usize::from(device_width).saturating_sub(2),
            )
        } else {
            "No controllers observed · r to rescan".to_string()
        };
        vec![ListItem::new(empty_state)]
    } else {
        items
    };
    let mut list_state = ListState::default();
    if state.input_cursor < devices.len() {
        list_state.select(Some(state.input_cursor));
    }
    let candidate_is_selected = devices
        .get(state.input_cursor)
        .is_some_and(|device| selected_id.is_some_and(|selected| selected == device.id));
    let devices_title = live_joypad.map_or_else(
        || "Devices · Select / Enable / Disable / Rescan".to_string(),
        |joypad| {
            if let Some(reason) = joypad
                .value
                .unavailable_reason
                .as_deref()
                .filter(|_| !devices.is_empty())
            {
                let omitted = if joypad.value.devices_truncated == 0 {
                    String::new()
                } else {
                    format!(" · +{} omitted", joypad.value.devices_truncated)
                };
                sanitize_and_fit_cell(
                    &format!("Devices · Input unavailable · {reason}{omitted}"),
                    usize::from(device_width),
                )
            } else if joypad.value.devices_truncated == 0 {
                "Devices · Select / Enable / Disable / Rescan".to_string()
            } else {
                format!(
                    "Devices · {} shown · +{} omitted",
                    devices.len(),
                    joypad.value.devices_truncated
                )
            }
        },
    );
    frame.render_stateful_widget(
        List::new(items)
            .block(shell_block(theme, &devices_title))
            .highlight_style(if candidate_is_selected {
                color::selected(theme, Role::Accent)
            } else {
                color::candidate(theme, Role::Accent)
            })
            .highlight_symbol("› ")
            .highlight_spacing(HighlightSpacing::Always),
        sections[0],
        &mut list_state,
    );
    frame.render_widget(
        Paragraph::new(overview)
            .block(shell_block(theme, "Motion"))
            .wrap(Wrap { trim: true }),
        sections[1],
    );
}

fn short_device_id(id: &str) -> String {
    if id.chars().count() <= 14 {
        return id.to_string();
    }
    let prefix = id.chars().take(7).collect::<String>();
    let suffix = id
        .chars()
        .rev()
        .take(5)
        .collect::<String>()
        .chars()
        .rev()
        .collect::<String>();
    format!("{prefix}…{suffix}")
}

fn device_row(device: &crate::telemetry::JoypadDevice, selected: bool, inner_width: u16) -> String {
    let available = usize::from(inner_width).saturating_sub(2);
    let status = device_status_label(device.status);
    if available < 60 {
        let name_width = available.saturating_sub(status.len() + 5).max(1);
        let name = sanitize_and_fit_cell(&device.name, name_width);
        return format!("{} {name} · {status}", if selected { "●" } else { "○" });
    }
    let id = sanitize_and_fit_cell(&short_device_id(&device.id), 16);
    let name = sanitize_and_fit_cell(&device.name, 28);
    format!("{} {id} {name} {status}", if selected { "●" } else { "○" })
}

fn device_status_label(status: JoypadDeviceStatus) -> &'static str {
    match status {
        JoypadDeviceStatus::Ready => "Ready",
        JoypadDeviceStatus::Disconnected => "Disconnected",
        JoypadDeviceStatus::Unsupported => "Unsupported",
        JoypadDeviceStatus::Unknown => "Unknown",
    }
}

fn draw_footer(frame: &mut Frame, theme: Theme, state: &AppState, area: Rect) {
    let page_help = if state.navigation == NavigationLevel::Tabs {
        "←→ choose page · Enter open"
    } else {
        match state.page {
            Page::Overview => "Esc tabs",
            Page::Runtimes if state.runtime_detail_id.is_some() => {
                "Esc runtime list · l logs · r restart"
            }
            Page::Runtimes => "↑↓ choose runtime · Enter details · l logs · r restart · Esc tabs",
            Page::Logs => "←→ filters · Enter edit/change · ↑↓ scroll · Space pause · Esc tabs",
            Page::Bus => "←→ controls · Enter edit/change · ↑↓ scroll · Esc tabs",
            Page::Input => {
                "↑↓ choose device · Enter select · e enable · x disable · r rescan · Esc tabs"
            }
        }
    };
    let global_help = if area.width >= 104 {
        "i session info · ? help · q quit"
    } else if area.width >= 68 {
        "i info · ? help · q quit"
    } else {
        "? help · q quit"
    };
    let global_width = u16::try_from(global_help.chars().count())
        .unwrap_or(u16::MAX)
        .min(area.width);
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(0), Constraint::Length(global_width)])
        .split(area);
    frame.render_widget(
        Paragraph::new(format!(" {page_help}")).style(color::muted(theme)),
        columns[0],
    );
    frame.render_widget(
        Paragraph::new(global_help)
            .alignment(Alignment::Right)
            .style(color::muted(theme)),
        columns[1],
    );
}

fn draw_help(frame: &mut Frame, theme: Theme, area: Rect) {
    let lines = vec![
        Line::from("Arrows       move the soft cursor"),
        Line::from("Enter        open / activate"),
        Line::from("Esc          back one level"),
        Line::from("1-5          open a page directly"),
        Line::from("i            session information"),
        Line::from("? / Esc      close help"),
        Line::from("q / Ctrl-C   stop session"),
        Line::from(""),
        Line::from("More information"),
        Line::from("https://phoxal.com"),
        Line::from("Open an issue"),
        Line::from("github.com/phoxal/phoxal-cli/issues"),
    ];
    let help_height = if area.width < 70 { 17 } else { 15 };
    let target = centered_fixed(area, area.width.min(70), area.height.min(help_height));
    frame.render_widget(Clear, target);
    frame.render_widget(
        Paragraph::new(lines)
            .block(shell_block(theme, "Help"))
            .wrap(Wrap { trim: true }),
        target,
    );
}

fn draw_session_info(
    frame: &mut Frame,
    theme: Theme,
    title: &TitleInfo,
    model: &SessionViewModel<'_>,
    area: Rect,
) {
    let manifest = sanitize_terminal_text(&title.manifest);
    let started = title.started_at.duration_since(UNIX_EPOCH).map_or_else(
        |_| "n/a".to_string(),
        |value| format!("unix {}", value.as_secs()),
    );
    let lines = vec![
        Line::from(format!(
            "robot            {}",
            sanitize_terminal_text(&title.robot)
        )),
        Line::from(format!(
            "namespace        {}",
            sanitize_terminal_text(&title.namespace)
        )),
        Line::from(format!("mode             {}", title.mode)),
        Line::from(format!("manifest         {manifest}")),
        Line::from(format!(
            "artifact channel {}",
            sanitize_terminal_text(&title.channel)
        )),
        Line::from(format!(
            "bus endpoint     {}",
            sanitize_terminal_text(&title.bus_endpoint)
        )),
        Line::from(format!("CLI              {}", env!("CARGO_PKG_VERSION"))),
        Line::from(format!(
            "start time       {started} · {} ago",
            human::duration(model.now.saturating_duration_since(title.started_instant))
        )),
    ];
    let info_height = if area.width < 70 { 15 } else { 13 };
    let target = centered_fixed(area, area.width.min(74), area.height.min(info_height));
    frame.render_widget(Clear, target);
    frame.render_widget(
        Paragraph::new(lines)
            .block(shell_block(theme, "Session Information · i/Esc close"))
            .wrap(Wrap { trim: true }),
        target,
    );
}

fn severity_label(severity: LogSeverity) -> &'static str {
    match severity {
        LogSeverity::Trace => "TRACE",
        LogSeverity::Debug => "DEBUG",
        LogSeverity::Info => "INFO",
        LogSeverity::Warn => "WARN",
        LogSeverity::Error => "ERROR",
    }
}

fn empty_as_all(value: &str) -> &str {
    if value.is_empty() { "all" } else { value }
}

fn ellipsize(text: &str, width: usize) -> String {
    if UnicodeWidthStr::width(text) <= width {
        return text.to_string();
    }
    if width == 0 {
        return String::new();
    }
    if width == 1 {
        return "…".to_string();
    }
    let budget = width - 1;
    let mut used: usize = 0;
    let mut shortened = String::new();
    for character in text.chars() {
        let character_width = UnicodeWidthChar::width(character).unwrap_or(0);
        if used.saturating_add(character_width) > budget {
            break;
        }
        shortened.push(character);
        used = used.saturating_add(character_width);
    }
    shortened.push('…');
    shortened
}

fn sanitize_and_ellipsize(text: &str, width: usize) -> String {
    ellipsize(&sanitize_terminal_text(text), width)
}

fn fit_cell(text: &str, width: usize) -> String {
    let mut fitted = ellipsize(text, width);
    fitted.push_str(&" ".repeat(width.saturating_sub(UnicodeWidthStr::width(fitted.as_str()))));
    fitted
}

fn sanitize_and_fit_cell(text: &str, width: usize) -> String {
    fit_cell(&sanitize_terminal_text(text), width)
}

fn shell_block<'a>(theme: Theme, title: &'a str) -> Block<'a> {
    Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(color::fg(theme, Role::Border))
}

fn centered(area: Rect, width_percent: u16, height_percent: u16) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - height_percent) / 2),
            Constraint::Percentage(height_percent),
            Constraint::Percentage((100 - height_percent) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - width_percent) / 2),
            Constraint::Percentage(width_percent),
            Constraint::Percentage((100 - width_percent) / 2),
        ])
        .split(vertical[1])[1]
}

fn centered_fixed(area: Rect, width: u16, height: u16) -> Rect {
    Rect::new(
        area.x.saturating_add(area.width.saturating_sub(width) / 2),
        area.y
            .saturating_add(area.height.saturating_sub(height) / 2),
        width.min(area.width),
        height.min(area.height),
    )
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;
    use crate::stores::log_store::LogStore;
    use crate::stores::runtime_store::RuntimeStore;
    use crate::supervisor::{
        BoardSnapshot, LogSource, ParticipantState, ParticipantStatus, RoutedLogLine,
    };
    use crate::telemetry::{DiskSample, JoypadDevice};
    use phoxal_cli_core::session::ParticipantKind;

    fn title() -> TitleInfo {
        TitleInfo {
            robot: "rover".to_string(),
            namespace: "dev".to_string(),
            channel: "stable".to_string(),
            manifest: "./robot.yaml".to_string(),
            mode: SessionMode::Run,
            bus_endpoint: "tcp/localhost:7447".to_string(),
            started_at: UNIX_EPOCH,
            started_instant: Instant::now(),
        }
    }

    #[test]
    fn startup_phase_text_is_sanitized_and_bounded() {
        let phase = PhaseRow {
            id: crate::session::event::PhaseId::new("prepare"),
            label: format!("{}\u{1b}[2J", "phase".repeat(20)),
            progress: Some(crate::tui::startup::PhaseProgressInfo {
                completed: 1,
                total: 2,
                detail: Some(format!("{}\u{1b}]0;owned\u{7}", "detail".repeat(20))),
            }),
            outcome: Some((
                PhaseOutcome::Failed {
                    error: format!("{}\u{1b}[7;39H", "failure".repeat(20)),
                },
                Duration::from_secs(1),
            )),
        };
        let rendered = startup_phase_lines(&phase)
            .into_iter()
            .map(|line| line.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!rendered.contains('\u{1b}'));
        assert!(rendered.lines().all(|line| line.chars().count() < 100));
    }

    fn render_page(page: Page, telemetry: &TelemetrySnapshot) -> String {
        render_page_at(page, telemetry, 100, 28)
    }

    fn render_page_at(
        page: Page,
        telemetry: &TelemetrySnapshot,
        width: u16,
        height: u16,
    ) -> String {
        let board = BoardSnapshot::default();
        let logs = LogStore::new();
        let runtime = RuntimeStore::new();
        let model = SessionViewModel::new(&board, &logs, &runtime, telemetry, Instant::now());
        let mut state = AppState::default();
        state.page = page;
        render_model(&title(), &state, &model, width, height)
    }

    fn render_model(
        title: &TitleInfo,
        state: &AppState,
        model: &SessionViewModel<'_>,
        width: u16,
        height: u16,
    ) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut render_state = state.clone();
        terminal
            .draw(|frame| {
                draw(
                    frame,
                    Theme::new(ColorCapability::None),
                    title,
                    &mut render_state,
                    model,
                );
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn render_startup_at(width: u16, height: u16) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        let startup = StartupState::new();
        let state = AppState::default();
        let telemetry = TelemetrySnapshot::default();
        let title = title();
        terminal
            .draw(|frame| {
                draw_startup(
                    frame,
                    Theme::new(ColorCapability::None),
                    &StartupView {
                        title: &title,
                        startup: &startup,
                        state: &state,
                        telemetry: &telemetry,
                        now: Instant::now(),
                    },
                );
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn startup_surface_renders_normal_and_too_small_layouts() {
        let normal = render_startup_at(100, 28);
        assert!(normal.contains("Starting session"), "{normal}");
        assert!(normal.contains("preparing"), "{normal}");
        let small = render_startup_at(43, 17);
        assert!(small.contains("Terminal too small"), "{small}");
    }

    #[test]
    fn every_fixed_page_renders_with_empty_data() {
        for page in Page::ALL {
            let rendered = render_page(page, &TelemetrySnapshot::default());
            assert!(rendered.contains(page.label()), "{page:?}: {rendered}");
            assert!(rendered.contains("Overview"));
            assert!(rendered.contains("Runtimes"));
            assert!(rendered.contains("Logs"));
            assert!(rendered.contains("Bus"));
            assert!(rendered.contains("Input"));
        }
    }

    #[test]
    fn responsive_header_and_tabs_cover_compact_expanded_and_too_small_sizes() {
        let now = Instant::now();
        let telemetry = TelemetrySnapshot {
            host: Some(Timestamped {
                received_at: now,
                value: HostSample {
                    cpu_pct: 10.0,
                    ram_used_bytes: 2,
                    ram_total_bytes: 4,
                    load_1m: 0.1,
                    load_5m: 0.2,
                    load_15m: 0.3,
                    disks: vec![DiskSample {
                        mount_point: "/".to_string(),
                        used_bytes: 10,
                        total_bytes: 100,
                        ..DiskSample::default()
                    }]
                    .into(),
                    ..HostSample::default()
                },
            }),
            clock: Some(Timestamped {
                received_at: now,
                value: ClockSample {
                    now_ns: 5_000_000_000,
                    step: 42,
                },
            }),
            ..TelemetrySnapshot::default()
        };
        let board = BoardSnapshot::default();
        let logs = LogStore::new();
        let runtime = RuntimeStore::new();
        let model = SessionViewModel::new(&board, &logs, &runtime, &telemetry, now);
        let mut simulation_title = title();
        simulation_title.mode = SessionMode::Simulation;

        let compact = render_model(&simulation_title, &AppState::default(), &model, 44, 18);
        assert!(compact.contains("cpu 10%"), "{compact}");
        assert!(compact.contains("step 42"), "{compact}");
        assert!(compact.contains("Input"), "{compact}");

        let expanded = render_model(&simulation_title, &AppState::default(), &model, 80, 24);
        assert!(expanded.contains("Host"), "{expanded}");
        assert!(expanded.contains("Simulation"), "{expanded}");
        assert!(expanded.contains("DISK (root)"), "{expanded}");

        let too_small = render_model(&simulation_title, &AppState::default(), &model, 44, 12);
        assert!(too_small.contains("Resize to at least 44 x 18"));
    }

    #[test]
    fn help_renders_product_and_issue_links() {
        let telemetry = TelemetrySnapshot::default();
        let board = BoardSnapshot::default();
        let logs = LogStore::new();
        let runtime = RuntimeStore::new();
        let model = SessionViewModel::new(&board, &logs, &runtime, &telemetry, Instant::now());
        let mut state = AppState::default();
        state.show_help = true;
        let rendered = render_model(&title(), &state, &model, 80, 24);
        assert!(rendered.contains("https://phoxal.com"));
        assert!(rendered.contains("github.com/phoxal/phoxal-cli/issues"));

        let compact = render_model(&title(), &state, &model, 44, 18);
        let without_whitespace = compact
            .chars()
            .filter(|character| character.is_ascii() && !character.is_whitespace())
            .collect::<String>();
        assert!(
            without_whitespace.contains("github.com/phoxal/phoxal-cli/issues"),
            "{compact}"
        );

        state.show_help = false;
        state.show_info = true;
        let compact = render_model(&title(), &state, &model, 44, 18);
        assert!(compact.contains("start time"), "{compact}");
    }

    #[test]
    fn narrow_pages_keep_selected_controls_and_global_help_visible() {
        let telemetry = TelemetrySnapshot::default();
        let board = BoardSnapshot::default();
        let logs = LogStore::new();
        let runtime = RuntimeStore::new();
        let model = SessionViewModel::new(&board, &logs, &runtime, &telemetry, Instant::now());

        let mut logs_state = AppState::default();
        logs_state.page = Page::Logs;
        logs_state.navigation = NavigationLevel::Page;
        logs_state.log_filter_cursor = 3;
        logs_state.log_text_filter = "needle".to_string();
        let rendered = render_model(&title(), &logs_state, &model, 44, 18);
        assert!(rendered.contains("4/5 Contains: needle"), "{rendered}");
        assert!(rendered.contains("? help · q quit"), "{rendered}");

        logs_state.page = Page::Bus;
        logs_state.bus_control_cursor = 2;
        logs_state.bus_show_internal = true;
        let rendered = render_model(&title(), &logs_state, &model, 44, 18);
        assert!(
            rendered.contains("3/3 Internal topics: Shown"),
            "{rendered}"
        );
        assert!(rendered.contains("? help · q quit"), "{rendered}");
    }

    #[test]
    fn tools_source_filter_renders_tool_logs() {
        let board = BoardSnapshot::default();
        let mut logs = LogStore::new();
        logs.record(RoutedLogLine {
            participant: SITE_TOOL_JOYPAD.to_string(),
            source: LogSource::Bus,
            severity: LogSeverity::Info,
            text: "joypad ready".to_string(),
        });
        logs.record(RoutedLogLine {
            participant: "phoxal-cli/Cli".to_string(),
            source: LogSource::Raw,
            severity: LogSeverity::Info,
            text: "cli diagnostic".to_string(),
        });
        let runtime = RuntimeStore::new();
        let telemetry = TelemetrySnapshot::default();
        let model = SessionViewModel::new(&board, &logs, &runtime, &telemetry, Instant::now());
        let mut state = AppState::default();
        state.page = Page::Logs;
        state.log_source_filter = LogSourceFilter::Tools;
        let rendered = render_model(&title(), &state, &model, 100, 28);
        assert!(rendered.contains("Source: Tools"));
        assert!(rendered.contains("joypad ready"));
        assert!(rendered.contains("cli diagnostic"));
    }

    #[test]
    fn runtime_page_keeps_the_three_group_boxes_when_empty() {
        let rendered = render_page(Page::Runtimes, &TelemetrySnapshot::default());
        for label in ["User services", "Framework services", "Drivers"] {
            assert!(rendered.contains(label), "missing {label}: {rendered}");
        }
        assert!(rendered.contains("No user runtimes"));
    }

    #[test]
    fn runtime_group_layout_preserves_every_box_and_distributes_extra_rows() {
        assert_eq!(runtime_section_heights([0, 0, 0], 12), [4, 4, 4]);
        assert_eq!(runtime_section_heights([0, 0, 0], 15), [5, 5, 5]);
        let distributed = runtime_section_heights([0, 12, 6], 20);
        assert_eq!(distributed.iter().sum::<u16>(), 20);
        assert!(distributed.into_iter().all(|height| height >= 4));
        assert!(distributed[1] > distributed[0]);
        assert!(distributed[2] > distributed[0]);
    }

    #[test]
    fn clipped_runtime_groups_report_how_many_rows_are_shown() {
        let mut board = BoardSnapshot::default();
        for id in ["alpha", "beta", "gamma", "delta"] {
            board.participants.insert(
                id.to_string(),
                ParticipantStatus::new(id, ParticipantKind::Service, ParticipantState::Ready),
            );
        }
        let logs = LogStore::new();
        let runtime = RuntimeStore::new();
        let telemetry = TelemetrySnapshot::default();
        let model = SessionViewModel::new(&board, &logs, &runtime, &telemetry, Instant::now());
        let mut state = AppState::default();
        state.page = Page::Runtimes;
        state.navigation = NavigationLevel::Page;

        let rendered = render_model(&title(), &state, &model, 44, 18);
        assert!(
            rendered.contains("Framework services · 1-1 of 4"),
            "{rendered}"
        );
    }

    #[test]
    fn runtime_header_stays_aligned_when_the_row_is_selected() {
        let mut board = BoardSnapshot::default();
        board.participants.insert(
            "alpha".to_string(),
            ParticipantStatus::new("alpha", ParticipantKind::Service, ParticipantState::Ready),
        );
        let logs = LogStore::new();
        let runtime = RuntimeStore::new();
        let telemetry = TelemetrySnapshot::default();
        let model = SessionViewModel::new(&board, &logs, &runtime, &telemetry, Instant::now());
        let mut state = AppState::default();
        state.page = Page::Runtimes;
        state.navigation = NavigationLevel::Page;
        let rendered = render_model(&title(), &state, &model, 100, 28);
        let header = rendered.lines().find(|line| line.contains("ID")).unwrap();
        let row = rendered
            .lines()
            .find(|line| line.contains("alpha"))
            .unwrap();
        assert_eq!(
            header.chars().position(|character| character == 'I'),
            row.chars().position(|character| character == 'a'),
            "{rendered}"
        );
    }

    #[test]
    fn simulation_runtime_page_replaces_driver_rows_with_placeholder() {
        let mut board = BoardSnapshot::default();
        board.participants.insert(
            "front_camera".to_string(),
            ParticipantStatus::new(
                "front_camera",
                ParticipantKind::Driver,
                ParticipantState::Degraded,
            ),
        );
        let logs = LogStore::new();
        let runtime = RuntimeStore::new();
        let telemetry = TelemetrySnapshot::default();
        let model = SessionViewModel::new(&board, &logs, &runtime, &telemetry, Instant::now());
        let mut state = AppState::for_mode(SessionMode::Simulation);
        state.page = Page::Runtimes;
        let mut simulation_title = title();
        simulation_title.mode = SessionMode::Simulation;
        let rendered = render_model(&simulation_title, &state, &model, 100, 28);

        assert!(rendered.contains("Not loaded in simulation"));
        assert!(!rendered.contains("front_camera"));

        state.page = Page::Overview;
        let rendered = render_model(&simulation_title, &state, &model, 100, 28);
        assert!(rendered.contains("degraded 0"), "{rendered}");
        assert!(!rendered.contains("front_camera"), "{rendered}");
    }

    #[test]
    fn bus_page_renders_total_history_and_per_producer_rate() {
        let now = Instant::now();
        let telemetry = TelemetrySnapshot {
            router: Some(Timestamped {
                received_at: now,
                value: crate::telemetry::RouterMetricsSample {
                    topics: vec![TopicMetric {
                        topic: "dev/robots/rover/v1/motion/state".to_string(),
                        from_participant: "motion".to_string(),
                        ingress_rate_hz: 12.0,
                        count: 99,
                        aggregate_overflow: false,
                    }]
                    .into(),
                    topics_truncated: 0,
                    throughput_msg_s: 12.0,
                    window_ns: 1_000_000_000,
                },
            }),
            router_throughput_history: vec![Timestamped {
                received_at: now,
                value: 12.0,
            }],
            ..TelemetrySnapshot::default()
        };
        let rendered = render_page(Page::Bus, &telemetry);
        assert!(rendered.contains("12.0 messages/s"));
        assert!(rendered.contains("All producers"));
        assert!(rendered.contains("motion"));

        let compact = render_page_at(Page::Bus, &telemetry, 44, 18);
        assert!(compact.contains("12.0 Hz"), "{compact}");
        assert!(compact.contains("99"), "{compact}");
    }

    #[test]
    fn bus_producer_summary_aggregates_all_topics_for_one_runtime() {
        let now = Instant::now();
        let telemetry = TelemetrySnapshot {
            router: Some(Timestamped {
                received_at: now,
                value: crate::telemetry::RouterMetricsSample {
                    topics: vec![
                        TopicMetric {
                            topic: "v1/motion/state".to_string(),
                            from_participant: "motion".to_string(),
                            ingress_rate_hz: 2.0,
                            count: 2,
                            aggregate_overflow: false,
                        },
                        TopicMetric {
                            topic: "v1/motion/target".to_string(),
                            from_participant: "motion".to_string(),
                            ingress_rate_hz: 3.0,
                            count: 3,
                            aggregate_overflow: false,
                        },
                    ]
                    .into(),
                    throughput_msg_s: 5.0,
                    ..crate::telemetry::RouterMetricsSample::default()
                },
            }),
            ..TelemetrySnapshot::default()
        };

        let rendered = render_page_at(Page::Bus, &telemetry, 100, 28);
        let producer_row = rendered
            .lines()
            .find(|line| line.contains("motion") && line.contains("5.0"))
            .expect("aggregated producer row");
        assert!(producer_row.contains('5'), "{producer_row}");
    }

    #[test]
    fn bus_producer_summary_discloses_capped_traffic_without_inventing_a_producer() {
        let now = Instant::now();
        let telemetry = TelemetrySnapshot {
            router: Some(Timestamped {
                received_at: now,
                value: crate::telemetry::RouterMetricsSample {
                    topics: vec![TopicMetric {
                        topic: "Other/unobserved traffic".to_string(),
                        from_participant: "multiple".to_string(),
                        ingress_rate_hz: 4.0,
                        count: 20,
                        aggregate_overflow: true,
                    }]
                    .into(),
                    topics_truncated: 0,
                    throughput_msg_s: 4.0,
                    window_ns: 1_000_000_000,
                },
            }),
            ..TelemetrySnapshot::default()
        };
        let rendered = render_page_at(Page::Bus, &telemetry, 100, 28);
        assert!(
            rendered.contains("Overflow excluded; total still includes it"),
            "{rendered}"
        );
        assert!(rendered.contains("Other/unobserved traffic"), "{rendered}");
        assert!(rendered.contains("1/1 visible"), "{rendered}");
        assert!(rendered.contains("aggregate"), "{rendered}");
        assert!(!rendered.contains("multiple"), "{rendered}");
    }

    #[test]
    fn input_page_keeps_tool_errors_in_logs_only() {
        let now = Instant::now();
        let telemetry = TelemetrySnapshot {
            joypad: Some(Timestamped {
                received_at: now,
                value: JoypadDevicesSample {
                    last_error: Some("selection failed".to_string()),
                    ..JoypadDevicesSample::default()
                },
            }),
            ..TelemetrySnapshot::default()
        };
        let rendered = render_page(Page::Input, &telemetry);
        assert!(!rendered.contains("selection failed"));
        assert!(!rendered.contains("Last error"));

        let unavailable = TelemetrySnapshot {
            joypad: Some(Timestamped {
                received_at: now,
                value: JoypadDevicesSample {
                    unavailable_reason: Some("gamepad backend unavailable".to_string()),
                    ..JoypadDevicesSample::default()
                },
            }),
            ..TelemetrySnapshot::default()
        };
        let rendered = render_page(Page::Input, &unavailable);
        assert!(
            rendered.contains("Input unavailable · gamepad backend unavailable"),
            "{rendered}"
        );

        let unavailable_with_device = TelemetrySnapshot {
            joypad: Some(Timestamped {
                received_at: now,
                value: JoypadDevicesSample {
                    available: vec![JoypadDevice {
                        id: "pad".to_string(),
                        name: "Pad".to_string(),
                        status: JoypadDeviceStatus::Ready,
                    }]
                    .into(),
                    unavailable_reason: Some(
                        "manual input requires differential kinematics".to_string(),
                    ),
                    ..JoypadDevicesSample::default()
                },
            }),
            ..TelemetrySnapshot::default()
        };
        let rendered = render_page(Page::Input, &unavailable_with_device);
        assert!(
            rendered.contains("Devices · Input unavailable"),
            "{rendered}"
        );
        assert!(rendered.contains("manual input requires"), "{rendered}");
    }

    #[test]
    fn motion_panel_stays_compact_when_input_state_is_stale() {
        let now = Instant::now();
        let old = now - DEFAULT_FRESHNESS_TTL - Duration::from_secs(1);
        let telemetry = TelemetrySnapshot {
            joypad: Some(Timestamped {
                received_at: old,
                value: JoypadDevicesSample {
                    available: vec![JoypadDevice {
                        id: "stale-pad".to_string(),
                        name: "Stale Pad".to_string(),
                        status: JoypadDeviceStatus::Ready,
                    }]
                    .into(),
                    selected: Some("stale-pad".to_string()),
                    enabled: true,
                    ..JoypadDevicesSample::default()
                },
            }),
            motion: Some(Timestamped {
                received_at: old,
                value: phoxal_api::v1::motion::State {
                    manual_candidate_age_ns: None,
                    autonomous_candidate_age_ns: None,
                    safety_constraints_age_ns: None,
                    selected_source: None,
                    final_target: phoxal_api::v1::motion::Target {
                        linear_x_mps: 0.0,
                        angular_z_radps: 0.0,
                        curvature_limit_radpm: None,
                    },
                    zero_reason: None,
                    safety_runtime: phoxal_api::v1::motion::SafetyRuntime::Absent,
                    software_estop_engaged: false,
                    component_estop_blocked: false,
                    active_safety_constraints: Vec::new(),
                },
            }),
            ..TelemetrySnapshot::default()
        };
        let mut board = BoardSnapshot::default();
        board.participants.insert(
            SITE_TOOL_JOYPAD.to_string(),
            ParticipantStatus::new(
                SITE_TOOL_JOYPAD,
                ParticipantKind::Tool,
                ParticipantState::Ready,
            ),
        );
        let mut runtime = RuntimeStore::new();
        runtime.observe_board(
            &board,
            &BTreeMap::from([(SITE_TOOL_JOYPAD.to_string(), now)]),
        );
        let logs = LogStore::new();
        let model = SessionViewModel::new(&board, &logs, &runtime, &telemetry, now);
        let mut state = AppState::default();
        state.page = Page::Input;
        let rendered = render_model(&title(), &state, &model, 100, 28);
        assert!(rendered.contains("Motion"));
        assert!(rendered.contains("Command"));
        assert!(rendered.contains("Motion update"));
        assert!(
            rendered
                .lines()
                .any(|line| line.contains("angular 0.000 rad/s")),
            "{rendered}"
        );
        assert!(rendered.contains("stale"));
        for removed in [
            "Manual control",
            "Selected",
            "Connection",
            "Command source",
            "Joypad heartbeat",
            "Device state",
            "Robot model",
        ] {
            assert!(
                !rendered.contains(removed),
                "unexpected {removed}: {rendered}"
            );
        }
    }

    #[test]
    fn simulation_pause_does_not_age_joypad_from_logical_clock() {
        let now = Instant::now();
        let telemetry = TelemetrySnapshot {
            clock: Some(Timestamped {
                received_at: now,
                value: ClockSample { now_ns: 0, step: 7 },
            }),
            joypad: Some(Timestamped {
                received_at: now,
                value: JoypadDevicesSample::default(),
            }),
            ..TelemetrySnapshot::default()
        };
        let mut board = BoardSnapshot::default();
        board.participants.insert(
            SITE_TOOL_JOYPAD.to_string(),
            ParticipantStatus::new(
                SITE_TOOL_JOYPAD,
                ParticipantKind::Tool,
                ParticipantState::Ready,
            ),
        );
        let mut runtime = RuntimeStore::new();
        runtime.observe_board(
            &board,
            &BTreeMap::from([(SITE_TOOL_JOYPAD.to_string(), now)]),
        );
        let logs = LogStore::new();
        let model = SessionViewModel::new(&board, &logs, &runtime, &telemetry, now);
        let mut state = AppState::for_mode(SessionMode::Simulation);
        state.page = Page::Input;
        let mut sim_title = title();
        sim_title.mode = SessionMode::Simulation;

        let rendered = render_model(&sim_title, &state, &model, 100, 28);
        assert!(rendered.contains("step    7"), "{rendered}");
        assert!(rendered.contains("Motion"), "{rendered}");
    }

    #[test]
    fn stale_simulation_clock_is_presented_as_paused() {
        let now = Instant::now();
        let telemetry = TelemetrySnapshot {
            clock: Some(Timestamped {
                received_at: now - DEFAULT_FRESHNESS_TTL - Duration::from_millis(1),
                value: ClockSample {
                    now_ns: 2_000_000_000,
                    step: 9,
                },
            }),
            ..TelemetrySnapshot::default()
        };
        let board = BoardSnapshot::default();
        let logs = LogStore::new();
        let runtime = RuntimeStore::new();
        let model = SessionViewModel::new(&board, &logs, &runtime, &telemetry, now);
        let mut simulation_title = title();
        simulation_title.mode = SessionMode::Simulation;

        let rendered = render_model(&simulation_title, &AppState::default(), &model, 100, 28);
        assert!(rendered.contains("state   paused"), "{rendered}");
        assert!(rendered.contains("step    9"), "{rendered}");
    }

    #[test]
    fn header_renders_root_disk_and_staleness() {
        let old = Instant::now() - DEFAULT_FRESHNESS_TTL - Duration::from_secs(1);
        let host = Timestamped {
            received_at: old,
            value: HostSample {
                cpu_pct: 10.0,
                ram_used_bytes: 2,
                ram_total_bytes: 4,
                load_1m: 0.1,
                load_5m: 0.2,
                load_15m: 0.3,
                uptime_s: Some(65),
                disks: vec![DiskSample {
                    mount_point: "/".to_string(),
                    file_system: "apfs".to_string(),
                    used_bytes: 10,
                    total_bytes: 100,
                }]
                .into(),
                ..HostSample::default()
            },
        };
        let lines = header_host_lines(Some(&host), Instant::now(), 40)
            .into_iter()
            .map(|line| line.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(lines.contains("DISK (root)"));
        assert!(lines.contains("stale"));

        let telemetry = TelemetrySnapshot {
            host: Some(host),
            ..TelemetrySnapshot::default()
        };
        let rendered = render_page_at(Page::Overview, &telemetry, 80, 24);
        assert!(rendered.contains("0.1/0.2/0.3"), "{rendered}");
    }

    #[test]
    fn bus_sort_is_deterministic_for_rate_topic_and_producer() {
        let a = TopicMetric {
            topic: "z/topic".to_string(),
            from_participant: "alpha".to_string(),
            ingress_rate_hz: 1.0,
            count: 1,
            aggregate_overflow: false,
        };
        let b = TopicMetric {
            topic: "a/topic".to_string(),
            from_participant: "zeta".to_string(),
            ingress_rate_hz: 5.0,
            count: 2,
            aggregate_overflow: false,
        };
        let mut topics = vec![&a, &b];
        sort_topics(&mut topics, BusSort::Rate);
        assert_eq!(topics[0].topic, "a/topic");
        sort_topics(&mut topics, BusSort::Topic);
        assert_eq!(topics[0].topic, "a/topic");
        sort_topics(&mut topics, BusSort::Producer);
        assert_eq!(topics[0].from_participant, "alpha");
    }

    #[test]
    fn overscroll_keeps_a_full_oldest_window_visible() {
        assert_eq!(bounded_window_start(usize::MAX, 10, 4), 6);
        assert_eq!(bounded_window_start(usize::MAX, 3, 4), 0);
    }

    #[test]
    fn remote_cell_text_is_sanitized_and_ellipsized() {
        assert_eq!(sanitize_and_ellipsize("alpha\u{1b}[2Jbeta", 8), "alphabe…");
        assert_eq!(
            sanitize_and_ellipsize("alpha\u{202e}beta", 10),
            "alpha beta"
        );
        assert_eq!(ellipsize("controller", 1), "…");
        assert_eq!(ellipsize("pad", 8), "pad");
        assert_eq!(ellipsize("控制器", 5), "控制…");
        assert_eq!(
            UnicodeWidthStr::width(sanitize_and_fit_cell("控制", 6).as_str()),
            6
        );
    }

    #[test]
    fn bus_history_title_describes_the_observed_window() {
        let now = Instant::now();
        assert_eq!(history_span(&[]), "waiting");
        assert_eq!(
            history_span(&[Timestamped {
                received_at: now,
                value: 1.0,
            }],),
            "1 sample"
        );
        assert_eq!(
            history_span(&[
                Timestamped {
                    received_at: now - Duration::from_secs(8),
                    value: 1.0,
                },
                Timestamped {
                    received_at: now,
                    value: 2.0,
                },
            ],),
            "last 8.0s"
        );
    }

    #[test]
    fn bus_history_uses_the_newest_samples_that_fit_the_graph() {
        let history = [1, 2, 3, 4, 5];
        assert_eq!(history_tail(&history, 3), [3, 4, 5]);
        assert_eq!(history_tail(&history, 10), history);
    }

    #[test]
    fn bus_graph_title_matches_the_samples_that_fit_the_panel() {
        let now = Instant::now();
        let telemetry = TelemetrySnapshot {
            router: Some(Timestamped {
                received_at: now,
                value: crate::telemetry::RouterMetricsSample {
                    throughput_msg_s: 59.0,
                    ..crate::telemetry::RouterMetricsSample::default()
                },
            }),
            router_throughput_history: (0..60)
                .map(|seconds| Timestamped {
                    received_at: now - Duration::from_secs(59 - seconds),
                    value: seconds as f32,
                })
                .collect(),
            ..TelemetrySnapshot::default()
        };
        let rendered = render_page_at(Page::Bus, &telemetry, 120, 28);
        assert!(rendered.contains("last 51.0s"), "{rendered}");
    }

    #[test]
    fn persistent_header_sanitizes_identity_fields() {
        let telemetry = TelemetrySnapshot::default();
        let board = BoardSnapshot::default();
        let logs = LogStore::new();
        let runtime = RuntimeStore::new();
        let model = SessionViewModel::new(&board, &logs, &runtime, &telemetry, Instant::now());
        let mut unsafe_title = title();
        unsafe_title.robot = "rover\u{202e}spoof".to_string();
        unsafe_title.namespace = "dev\u{1b}[2J".to_string();
        let rendered = render_model(&unsafe_title, &AppState::default(), &model, 100, 28);
        assert!(!rendered.contains('\u{202e}'));
        assert!(!rendered.contains('\u{1b}'));
    }

    #[test]
    fn runtime_detail_sanitizes_and_bounds_remote_identity_text() {
        let unsafe_id = format!("runtime\u{202e}{}", "x".repeat(100));
        let mut board = BoardSnapshot::default();
        board.participants.insert(
            unsafe_id.clone(),
            ParticipantStatus::new(
                &unsafe_id,
                ParticipantKind::Service,
                ParticipantState::Ready,
            ),
        );
        let logs = LogStore::new();
        let runtime = RuntimeStore::new();
        let telemetry = TelemetrySnapshot::default();
        let model = SessionViewModel::new(&board, &logs, &runtime, &telemetry, Instant::now());
        let mut state = AppState::default();
        state.page = Page::Runtimes;
        state.navigation = NavigationLevel::Page;
        state.runtime_detail_id = Some(unsafe_id.clone());
        let rendered = render_model(&title(), &state, &model, 100, 28);
        assert!(!rendered.contains('\u{202e}'));
        assert!(!rendered.contains(&unsafe_id));
        assert!(rendered.contains("Identity"), "{rendered}");
    }

    #[test]
    fn input_page_uses_devices_and_compact_motion_panels() {
        let rendered = render_page(Page::Input, &TelemetrySnapshot::default());
        assert!(rendered.contains("Devices"));
        assert!(rendered.contains("Motion"));
        assert!(rendered.contains("Command"));
        assert!(rendered.contains("Motion update"));
    }

    #[test]
    fn small_terminal_degrades_to_resize_message() {
        let board = BoardSnapshot::default();
        let logs = LogStore::new();
        let runtime = RuntimeStore::new();
        let telemetry = TelemetrySnapshot::default();
        let model = SessionViewModel::new(&board, &logs, &runtime, &telemetry, Instant::now());
        let backend = TestBackend::new(30, 6);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut state = AppState::default();
        terminal
            .draw(|frame| {
                draw(
                    frame,
                    Theme::new(ColorCapability::None),
                    &title(),
                    &mut state,
                    &model,
                );
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        let text = (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("Terminal too small"));
    }
}
