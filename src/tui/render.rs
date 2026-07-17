//! Rendering for the fixed Overview, Runtimes, Logs, Bus, and Input pages.

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Borders, Clear, List, ListItem, ListState, Paragraph, Tabs, Wrap,
};

use crate::identity::IdentitySummary;
use crate::session::event::PhaseOutcome;
use crate::stores::telemetry_store::{DEFAULT_FRESHNESS_TTL, Timestamped};
use crate::supervisor::{ClockSample, LogSeverity, ParticipantStatus};
use crate::telemetry::{
    HostSample, JoypadDeviceStatus, JoypadDevicesSample, TelemetrySnapshot, TopicMetric,
};
use crate::theme::{Role, Theme, state_role, state_symbol};
use crate::tui::color;
use crate::tui::startup::{PhaseRow, StartupState};
use crate::tui::state::{AppState, BusSort, Page};
use crate::tui::view_model::SessionViewModel;
use crate::tui::visibility::is_internal_id;

#[derive(Debug, Clone)]
pub struct TitleInfo {
    pub robot: String,
    pub channel: String,
    pub mode: &'static str,
    pub bus_endpoint: String,
    pub started_at: SystemTime,
}

#[must_use]
pub fn simulation_clock_slot(mode: &str, clock: Option<ClockSample>) -> String {
    if mode != "simulation" {
        return "logical real time".to_string();
    }
    clock.map_or_else(
        || "logical n/a".to_string(),
        |sample| {
            format!(
                "step {} · {}",
                sample.step,
                crate::human::duration(Duration::from_nanos(sample.now_ns))
            )
        },
    )
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
        crate::human::bytes_compact(host.value.ram_used_bytes),
        crate::human::bytes_compact(host.value.ram_total_bytes),
    )
}

pub fn draw(
    frame: &mut Frame,
    theme: Theme,
    title: &TitleInfo,
    identity: Option<&IdentitySummary>,
    state: &AppState,
    model: &SessionViewModel<'_>,
) {
    let area = frame.area();
    frame.render_widget(Clear, area);
    if area.width < 44 || area.height < 9 {
        let message = Paragraph::new(vec![
            Line::from("Phoxal session"),
            Line::from("Terminal too small"),
            Line::from("Resize to at least 44 x 9"),
            Line::from(format!(
                "1 Overview  2 Runtimes  3 Logs  4 Bus  5 Input  [{}]",
                state.page.label()
            )),
        ])
        .block(shell_block(theme, "Session"))
        .wrap(Wrap { trim: true });
        frame.render_widget(message, area);
        return;
    }

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(1),
        ])
        .split(area);
    draw_title(frame, theme, title, model.telemetry, rows[0]);
    draw_tabs(frame, theme, state.page, rows[1]);
    draw_status(frame, theme, model, rows[2]);
    match state.page {
        Page::Overview => draw_overview(frame, theme, model, rows[3]),
        Page::Runtimes => draw_runtimes(frame, theme, state, model, rows[3]),
        Page::Logs => draw_logs(frame, theme, state, model, rows[3]),
        Page::Bus => draw_bus(frame, theme, state, model, rows[3]),
        Page::Input => draw_input(frame, theme, state, model, rows[3]),
    }
    draw_footer(frame, theme, state, rows[4]);
    if state.show_help {
        draw_help(frame, theme, area);
    }
    if state.show_info {
        draw_session_info(frame, theme, title, identity, model, area);
    }
}

pub fn draw_startup(
    frame: &mut Frame,
    theme: Theme,
    title: &TitleInfo,
    identity: Option<&IdentitySummary>,
    startup: &StartupState,
) {
    let area = frame.area();
    frame.render_widget(Clear, area);
    let mut lines = vec![
        Line::from(Span::styled("p h o x a l", color::fg(theme, Role::Accent))),
        Line::from(format!("robot      {}", title.robot)),
        Line::from(format!("mode       {}", title.mode)),
        Line::from(format!("environment {}", title.channel)),
    ];
    if let Some(identity) = identity {
        lines.push(Line::from(format!("manifest   {}", identity.manifest)));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(format!(
        "session    {}",
        startup.session_state.label()
    )));
    if let Some(phase) = &startup.phase {
        lines.extend(startup_phase_lines(phase));
    }
    lines.push(Line::from(""));
    lines.push(Line::from("? help · q quit"));
    frame.render_widget(
        Paragraph::new(lines)
            .block(shell_block(theme, "Starting session"))
            .wrap(Wrap { trim: true }),
        centered(area, 70, 70),
    );
}

fn startup_phase_lines(phase: &PhaseRow) -> Vec<Line<'static>> {
    let status = match &phase.outcome {
        None => "in progress".to_string(),
        Some((PhaseOutcome::Succeeded, elapsed)) => {
            format!("done in {}", crate::human::duration(*elapsed))
        }
        Some((PhaseOutcome::Skipped, _)) => "skipped".to_string(),
        Some((PhaseOutcome::Failed { error }, _)) => format!("failed: {error}"),
    };
    let mut lines = vec![Line::from(format!("phase      {} · {status}", phase.label))];
    if let Some(progress) = &phase.progress {
        lines.push(Line::from(format!(
            "progress   {}/{}{}",
            progress.completed,
            progress.total,
            progress
                .detail
                .as_deref()
                .map_or_else(String::new, |detail| format!(" · {detail}"))
        )));
    }
    lines
}

fn draw_title(
    frame: &mut Frame,
    theme: Theme,
    title: &TitleInfo,
    telemetry: &TelemetrySnapshot,
    area: Rect,
) {
    let text = format!(
        " {} · {} · env {}                                      {} ",
        title.robot,
        title.mode,
        title.channel,
        simulation_clock_slot(title.mode, telemetry.clock)
    );
    frame.render_widget(
        Paragraph::new(text).style(color::fg(theme, Role::TextPrimary)),
        area,
    );
}

fn draw_tabs(frame: &mut Frame, theme: Theme, page: Page, area: Rect) {
    let titles = Page::ALL.map(|page| Line::from(format!(" {:<10}", page.label())));
    frame.render_widget(
        Tabs::new(titles)
            .select(page.index())
            .style(color::muted(theme))
            .highlight_style(color::selected(theme, Role::Accent).add_modifier(Modifier::BOLD))
            .divider(""),
        area,
    );
}

fn draw_status(frame: &mut Frame, theme: Theme, model: &SessionViewModel<'_>, area: Rect) {
    let summary = model.summary;
    let text = format!(
        " ready {} · degraded {} · failed {} · starting {} · restarts {}             {} ",
        summary.ready,
        summary.degraded,
        summary.failed,
        summary.starting,
        summary.restarts,
        host_resource_slot(model.telemetry.host.as_ref(), model.now)
    );
    frame.render_widget(Paragraph::new(text).style(color::muted(theme)), area);
}

fn draw_overview(frame: &mut Frame, theme: Theme, model: &SessionViewModel<'_>, area: Rect) {
    let sections = if area.width >= 88 {
        Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(43), Constraint::Percentage(57)])
            .split(area)
    } else {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(9), Constraint::Min(4)])
            .split(area)
    };
    let host_lines = host_lines(model.telemetry.host.as_ref(), model.now);
    frame.render_widget(
        Paragraph::new(host_lines)
            .block(shell_block(theme, "Host"))
            .wrap(Wrap { trim: true }),
        sections[0],
    );

    let attention = model.needs_attention();
    let mut lines = vec![Line::from(format!(
        "ready {}  degraded {}  failed {}  starting {}  restarts {}",
        model.summary.ready,
        model.summary.degraded,
        model.summary.failed,
        model.summary.starting,
        model.summary.restarts
    ))];
    lines.push(Line::from(""));
    if attention.is_empty() {
        lines.push(Line::from(Span::styled(
            "✓ Nothing needs attention",
            color::fg(theme, Role::Success),
        )));
    } else {
        for status in attention {
            lines.push(runtime_attention_line(theme, status, model));
        }
    }
    frame.render_widget(
        Paragraph::new(lines)
            .block(shell_block(theme, "Runtime summary · Needs attention"))
            .wrap(Wrap { trim: true }),
        sections[1],
    );
}

fn host_lines(host: Option<&Timestamped<HostSample>>, now: Instant) -> Vec<Line<'static>> {
    let Some(host) = host else {
        return vec![
            Line::from("CPU      n/a"),
            Line::from("RAM      n/a"),
            Line::from("root     n/a"),
            Line::from("load     n/a"),
            Line::from("uptime   n/a"),
        ];
    };
    let suffix = if host.is_stale(now, DEFAULT_FRESHNESS_TTL) {
        " (stale)"
    } else {
        ""
    };
    let root = host
        .value
        .disks
        .iter()
        .find(|disk| disk.mount_point == "/")
        .map_or_else(
            || "n/a".to_string(),
            |disk| {
                format!(
                    "{}/{} {}",
                    crate::human::bytes_compact(disk.used_bytes),
                    crate::human::bytes_compact(disk.total_bytes),
                    disk.file_system
                )
            },
        );
    vec![
        Line::from(format!("CPU      {:.1}%{suffix}", host.value.cpu_pct)),
        Line::from(format!(
            "RAM      {}/{}{suffix}",
            crate::human::bytes_compact(host.value.ram_used_bytes),
            crate::human::bytes_compact(host.value.ram_total_bytes)
        )),
        Line::from(format!("root     {root}{suffix}")),
        Line::from(format!(
            "load     {:.2} / {:.2} / {:.2}{suffix}",
            host.value.load_1m, host.value.load_5m, host.value.load_15m
        )),
        Line::from(format!(
            "uptime   {}{suffix}",
            host.value.uptime_s.map_or_else(
                || "n/a".to_string(),
                |seconds| crate::human::duration(Duration::from_secs(seconds))
            )
        )),
    ]
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
    let note = status.note.as_deref().unwrap_or("");
    Line::from(vec![
        Span::styled(
            format!("{} {:<18}", state_symbol(status.state), status.id),
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
    if state.runtime_detail {
        draw_runtime_detail(frame, theme, state, model, area);
        return;
    }
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(1)])
        .split(area);
    frame.render_widget(
        Paragraph::new(
            " id                    state       cpu     rss       uptime   restarts  heartbeat",
        )
        .style(color::muted(theme)),
        rows[0],
    );
    if model.runtimes.is_empty() {
        frame.render_widget(
            Paragraph::new("No robot runtimes observed").block(shell_block(theme, "Runtimes")),
            rows[1],
        );
        return;
    }
    let items = model
        .runtimes
        .iter()
        .map(|status| ListItem::new(runtime_row(status, model)))
        .collect::<Vec<_>>();
    let mut list_state = ListState::default();
    list_state.select(Some(state.runtime_cursor));
    frame.render_stateful_widget(
        List::new(items)
            .block(shell_block(theme, "Runtimes"))
            .highlight_style(color::selected(theme, Role::Accent))
            .highlight_symbol("› "),
        rows[1],
        &mut list_state,
    );
}

fn runtime_row(status: &ParticipantStatus, model: &SessionViewModel<'_>) -> String {
    let process = model.telemetry.process_by_participant.get(&status.id);
    let process_stale =
        process.is_some_and(|sample| sample.is_stale(model.now, DEFAULT_FRESHNESS_TTL));
    let cpu = process.map_or_else(
        || "n/a".to_string(),
        |sample| {
            if process_stale {
                "stale".to_string()
            } else {
                format!("{:.1}%", sample.value.cpu_pct)
            }
        },
    );
    let rss = process.map_or_else(
        || "n/a".to_string(),
        |sample| crate::human::bytes_compact(sample.value.rss_bytes),
    );
    let observation = model.runtime.observation(&status.id);
    let uptime = observation.map_or_else(
        || "n/a".to_string(),
        |observation| crate::human::duration(observation.uptime(model.now)),
    );
    let restarts = observation.map_or(status.restart_count, |observation| {
        observation.displayed_restarts()
    });
    let heartbeat = observation
        .and_then(|observation| observation.last_seen_age(model.now))
        .map_or_else(
            || "n/a".to_string(),
            |age| format!("{} ago", crate::human::duration(age)),
        );
    format!(
        "{} {:<20} {:<10} {:<7} {:<9} {:<8} {:>8}  {}",
        state_symbol(status.state),
        status.id,
        status.state.label(),
        cpu,
        rss,
        uptime,
        restarts,
        heartbeat
    )
}

fn draw_runtime_detail(
    frame: &mut Frame,
    theme: Theme,
    state: &AppState,
    model: &SessionViewModel<'_>,
    area: Rect,
) {
    let Some(status) = model.runtimes.get(state.runtime_cursor) else {
        frame.render_widget(Paragraph::new("Runtime no longer present"), area);
        return;
    };
    let metadata = model.runtime.metadata(&status.id);
    let observation = model.runtime.observation(&status.id);
    let process = model.telemetry.process_by_participant.get(&status.id);
    let artifact = metadata
        .and_then(|metadata| metadata.artifact_ref.as_deref())
        .unwrap_or("n/a");
    let artifact_size = status
        .artifact_size_bytes
        .map_or_else(|| "n/a".to_string(), crate::human::bytes_compact);
    let pid = status
        .pid
        .map_or_else(|| "n/a".to_string(), |pid| pid.to_string());
    let ownership = metadata
        .map(|metadata| format!("{:?}", metadata.ownership))
        .unwrap_or_else(|| "n/a".to_string());
    let ready_after = model
        .runtime
        .time_to_ready(&status.id)
        .map_or_else(|| "n/a".to_string(), crate::human::duration);
    let uptime = observation.map_or_else(
        || "n/a".to_string(),
        |observation| crate::human::duration(observation.uptime(model.now)),
    );
    let last_seen = observation
        .and_then(|observation| observation.last_seen_age(model.now))
        .map_or_else(
            || "n/a".to_string(),
            |age| format!("{} ago", crate::human::duration(age)),
        );
    let cpu = process.map_or_else(
        || "n/a".to_string(),
        |process| format!("{:.1}%", process.value.cpu_pct),
    );
    let memory = process.map_or_else(
        || "n/a".to_string(),
        |process| crate::human::bytes_compact(process.value.rss_bytes),
    );
    let restarts = observation.map_or(status.restart_count, |observation| {
        observation.displayed_restarts()
    });
    let inputs = metadata
        .map(|metadata| metadata.input_contracts.join(", "))
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "none".to_string());
    let outputs = metadata
        .map(|metadata| metadata.output_contracts.join(", "))
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "none".to_string());
    let lines = vec![
        Line::from(format!(
            "state         {} ({})",
            status.state.label(),
            status.kind.label()
        )),
        Line::from(format!(
            "source        {}",
            if status.local { "local" } else { "catalog" }
        )),
        Line::from(format!("artifact      {artifact}")),
        Line::from(format!("artifact size {artifact_size}")),
        Line::from(format!("PID           {pid}")),
        Line::from(format!("ownership     {ownership}")),
        Line::from(format!("ready after   {ready_after}")),
        Line::from(format!("uptime        {uptime}")),
        Line::from(format!("last seen     {last_seen}")),
        Line::from(format!("CPU / memory  {cpu} / {memory}")),
        Line::from(format!("restarts      {restarts}")),
        Line::from(format!(
            "last error    {}",
            status.note.as_deref().unwrap_or("none")
        )),
        Line::from(format!("inputs        {inputs}")),
        Line::from(format!("outputs       {outputs}")),
    ];
    frame.render_widget(
        Paragraph::new(lines)
            .block(shell_block(theme, &format!("Runtime · {}", status.id)))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn draw_logs(
    frame: &mut Frame,
    theme: Theme,
    state: &AppState,
    model: &SessionViewModel<'_>,
    area: Rect,
) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(2), Constraint::Min(1)])
        .split(area);
    let editing = state
        .editing_label()
        .map_or(String::new(), |label| format!(" · editing {label}"));
    frame.render_widget(
        Paragraph::new(format!(
            " runtime [{}] · severity {} · text [{}] · {}{}\n / text  f runtime  s severity  Space follow/pause  End live",
            empty_as_all(&state.log_runtime_filter),
            state.log_severity.label(),
            empty_as_all(&state.log_text_filter),
            if state.log_follow { "following" } else { "paused" },
            editing
        ))
        .style(color::muted(theme)),
        rows[0],
    );
    let runtime_filter = state.log_runtime_filter.to_lowercase();
    let text_filter = state.log_text_filter.to_lowercase();
    let filtered = model
        .logs
        .lines()
        .filter(|line| !is_internal_id(&line.participant, model.board, model.runtime))
        .filter(|line| {
            runtime_filter.is_empty() || line.participant.to_lowercase().contains(&runtime_filter)
        })
        .filter(|line| text_filter.is_empty() || line.text.to_lowercase().contains(&text_filter))
        .filter(|line| state.log_severity.matches(line.severity))
        .collect::<Vec<_>>();
    let height = usize::from(rows[1].height.saturating_sub(2));
    let end = filtered
        .len()
        .saturating_sub(bounded_scroll_offset(state.log_scroll, filtered.len()));
    let start = end.saturating_sub(height);
    let lines = filtered[start..end]
        .iter()
        .map(|line| {
            ListItem::new(format!(
                "{:>5} {:<18} {}",
                severity_label(line.severity),
                line.participant,
                line.text
            ))
        })
        .collect::<Vec<_>>();
    let body = if lines.is_empty() {
        List::new(vec![ListItem::new("No matching robot-runtime logs")])
    } else {
        List::new(lines)
    };
    frame.render_widget(body.block(shell_block(theme, "Logs")), rows[1]);
}

fn draw_bus(
    frame: &mut Frame,
    theme: Theme,
    state: &AppState,
    model: &SessionViewModel<'_>,
    area: Rect,
) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(2), Constraint::Min(1)])
        .split(area);
    let sample = model.telemetry.router.as_ref();
    let freshness = sample.map_or_else(
        || "n/a".to_string(),
        |sample| {
            let age = model.now.saturating_duration_since(sample.received_at);
            if sample.is_stale(model.now, DEFAULT_FRESHNESS_TTL) {
                format!("stale · {} ago", crate::human::duration(age))
            } else {
                format!("{} ago", crate::human::duration(age))
            }
        },
    );
    let filter = state.bus_filter.to_lowercase();
    let reveal_internal = state.bus_show_internal || !filter.is_empty();
    frame.render_widget(
        Paragraph::new(format!(
            " throughput {} msg/s · freshness {freshness} · sort {} · filter [{}]\n / filter  s sort  a internals {}",
            sample.map_or(0.0, |sample| sample.value.throughput_msg_s),
            state.bus_sort.label(),
            empty_as_all(&state.bus_filter),
            if reveal_internal { "shown" } else { "hidden" }
        ))
        .style(color::muted(theme)),
        rows[0],
    );
    let Some(sample) = sample else {
        frame.render_widget(
            Paragraph::new("Router metrics unavailable").block(shell_block(theme, "Bus")),
            rows[1],
        );
        return;
    };
    let mut topics = sample
        .value
        .topics
        .iter()
        .filter(|metric| {
            reveal_internal || !is_internal_id(&metric.from_participant, model.board, model.runtime)
        })
        .filter(|metric| {
            filter.is_empty()
                || metric.topic.to_lowercase().contains(&filter)
                || metric.from_participant.to_lowercase().contains(&filter)
        })
        .collect::<Vec<_>>();
    sort_topics(&mut topics, state.bus_sort);
    let height = usize::from(rows[1].height.saturating_sub(3));
    let start = bounded_scroll_offset(state.bus_scroll, topics.len());
    let mut lines = vec![Line::from(
        "topic                                      producer              rate       count",
    )];
    lines.extend(topics.iter().skip(start).take(height).map(|metric| {
        Line::from(format!(
            "{:<42} {:<20} {:>7.1} Hz {:>9}",
            metric.topic, metric.from_participant, metric.ingress_rate_hz, metric.count
        ))
    }));
    frame.render_widget(
        Paragraph::new(lines)
            .block(shell_block(theme, "Bus"))
            .wrap(Wrap { trim: false }),
        rows[1],
    );
}

fn bounded_scroll_offset(offset: usize, item_count: usize) -> usize {
    offset.min(item_count.saturating_sub(1))
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
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(11), Constraint::Min(2)])
        .split(area);
    let joypad = model.telemetry.joypad.as_ref();
    let status = input_status(joypad, model.now);
    let selected = joypad
        .and_then(|joypad| joypad.value.selected.as_deref())
        .unwrap_or("none");
    let connection = selected_connection(joypad.map(|joypad| &joypad.value));
    let enabled = joypad.map_or("n/a", |joypad| {
        if joypad.value.enabled {
            "enabled"
        } else {
            "disabled"
        }
    });
    let (command, source, updated) = model.telemetry.motion.as_ref().map_or_else(
        || ("n/a".to_string(), "n/a".to_string(), "n/a".to_string()),
        |motion| {
            (
                format!(
                    "linear {:.3} m/s · angular {:.3} rad/s",
                    motion.value.final_target.linear_x_mps,
                    motion.value.final_target.angular_z_radps
                ),
                motion
                    .value
                    .selected_source
                    .map_or_else(|| "none".to_string(), |source| format!("{source:?}")),
                format!(
                    "{} ago{}",
                    crate::human::duration(model.now.saturating_duration_since(motion.received_at)),
                    if motion.is_stale(model.now, DEFAULT_FRESHNESS_TTL) {
                        " · stale"
                    } else {
                        ""
                    }
                ),
            )
        },
    );
    let last_error = joypad
        .and_then(|joypad| joypad.value.last_error.as_deref())
        .unwrap_or("none");
    let unavailable = joypad
        .and_then(|joypad| joypad.value.unavailable_reason.as_deref())
        .unwrap_or("none");
    let overview = vec![
        Line::from(format!("state       {status}")),
        Line::from(format!("manual      {enabled}")),
        Line::from(format!("selected    {selected}")),
        Line::from(format!("connection  {connection}")),
        Line::from(format!("command     {command}")),
        Line::from(format!("source      {source}")),
        Line::from(format!("last update {updated}")),
        Line::from(format!("unavailable {unavailable}")),
        Line::from(format!("last error  {last_error}")),
    ];
    frame.render_widget(
        Paragraph::new(overview).block(shell_block(theme, "Input")),
        rows[0],
    );
    let devices = joypad
        .map(|joypad| joypad.value.available.as_slice())
        .unwrap_or_default();
    let items = devices
        .iter()
        .map(|device| {
            let selected = joypad
                .and_then(|sample| sample.value.selected.as_deref())
                .is_some_and(|selected| selected == device.id);
            ListItem::new(format!(
                "{} {:<24} {:<34} {:?}",
                if selected { "●" } else { "○" },
                device.id,
                device.name,
                device.status
            ))
        })
        .collect::<Vec<_>>();
    let items = if items.is_empty() {
        vec![ListItem::new("No controllers observed · r to rescan")]
    } else {
        items
    };
    let mut list_state = ListState::default();
    if !devices.is_empty() {
        list_state.select(Some(state.input_cursor));
    }
    frame.render_stateful_widget(
        List::new(items)
            .block(shell_block(
                theme,
                "Devices · Enter select · e enable · x disable · r rescan",
            ))
            .highlight_style(color::selected(theme, Role::Accent))
            .highlight_symbol("› "),
        rows[1],
        &mut list_state,
    );
}

fn input_status(joypad: Option<&Timestamped<JoypadDevicesSample>>, now: Instant) -> &'static str {
    let Some(joypad) = joypad else {
        return "backend unavailable";
    };
    if joypad.is_stale(now, DEFAULT_FRESHNESS_TTL) {
        return "stale tool";
    }
    if joypad.value.unavailable_reason.is_some() {
        return "backend unavailable";
    }
    let Some(selected) = joypad.value.selected.as_deref() else {
        return if joypad.value.available.is_empty() {
            "no controller"
        } else if joypad
            .value
            .available
            .iter()
            .any(|device| device.status == JoypadDeviceStatus::Ready)
        {
            "compatible unselected"
        } else {
            "unsupported"
        };
    };
    match joypad
        .value
        .available
        .iter()
        .find(|device| device.id == selected)
        .map(|device| device.status)
    {
        Some(JoypadDeviceStatus::Ready) => "ready selected",
        Some(JoypadDeviceStatus::Disconnected) => "disconnected",
        Some(JoypadDeviceStatus::Unsupported | JoypadDeviceStatus::Unknown) | None => "unsupported",
    }
}

fn selected_connection(joypad: Option<&JoypadDevicesSample>) -> &'static str {
    let Some(joypad) = joypad else {
        return "n/a";
    };
    let Some(selected) = joypad.selected.as_deref() else {
        return "unselected";
    };
    match joypad
        .available
        .iter()
        .find(|device| device.id == selected)
        .map(|device| device.status)
    {
        Some(JoypadDeviceStatus::Ready) => "connected",
        Some(JoypadDeviceStatus::Disconnected) => "disconnected",
        Some(JoypadDeviceStatus::Unsupported) => "unsupported",
        Some(JoypadDeviceStatus::Unknown) | None => "unknown",
    }
}

fn draw_footer(frame: &mut Frame, theme: Theme, state: &AppState, area: Rect) {
    let page_help = match state.page {
        Page::Overview => "1-5 pages",
        Page::Runtimes => "↑↓ select · Enter details · l logs · r restart",
        Page::Logs => "/ text · f runtime · s severity · Space follow",
        Page::Bus => "/ filter · s sort · a internals",
        Page::Input => "Enter select · e enable · x disable · r rescan",
    };
    frame.render_widget(
        Paragraph::new(format!(
            " {page_help}                                      i session info · ? help · q quit "
        ))
        .style(color::muted(theme)),
        area,
    );
}

fn draw_help(frame: &mut Frame, theme: Theme, area: Rect) {
    let lines = vec![
        Line::from("1-5 / ←→   switch fixed page"),
        Line::from("i           Session Information"),
        Line::from("? / Esc     close help"),
        Line::from("q / Ctrl-C  stop session"),
        Line::from(""),
        Line::from("Runtimes: Enter details, l filtered logs, r restart"),
        Line::from("Logs: / text, f runtime, s severity, Space follow"),
        Line::from("Bus: / filter, s sort, a reveal internals"),
        Line::from("Input: Enter select, e enable, x disable, r rescan"),
    ];
    let target = centered(area, 72, 64);
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
    identity: Option<&IdentitySummary>,
    model: &SessionViewModel<'_>,
    area: Rect,
) {
    let manifest = identity.map_or("n/a", |identity| identity.manifest.as_str());
    let started = title.started_at.duration_since(UNIX_EPOCH).map_or_else(
        |_| "n/a".to_string(),
        |value| format!("unix {}", value.as_secs()),
    );
    let lines = vec![
        Line::from(format!("robot            {}", title.robot)),
        Line::from(format!("mode             {}", title.mode)),
        Line::from(format!("manifest         {manifest}")),
        Line::from(format!("environment      {}", title.channel)),
        Line::from(format!("artifact channel {}", title.channel)),
        Line::from(format!("bus endpoint     {}", title.bus_endpoint)),
        Line::from(format!("CLI              {}", env!("CARGO_PKG_VERSION"))),
        Line::from(format!(
            "start time       {started} · {} ago",
            crate::human::duration(model.runtime.session_uptime(model.now))
        )),
    ];
    let target = centered(area, 72, 58);
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

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;
    use crate::stores::log_store::LogStore;
    use crate::stores::runtime_store::RuntimeStore;
    use crate::supervisor::BoardSnapshot;
    use crate::telemetry::{DiskSample, JoypadDevice};
    use crate::theme::ColorCapability;

    fn title() -> TitleInfo {
        TitleInfo {
            robot: "rover".to_string(),
            channel: "dev".to_string(),
            mode: "run",
            bus_endpoint: "tcp/localhost:7447".to_string(),
            started_at: UNIX_EPOCH,
        }
    }

    fn render_page(page: Page, telemetry: &TelemetrySnapshot) -> String {
        let board = BoardSnapshot::default();
        let logs = LogStore::new();
        let runtime = RuntimeStore::new();
        let model = SessionViewModel::new(&board, &logs, &runtime, telemetry, Instant::now());
        let mut state = AppState::default();
        state.page = page;
        let backend = TestBackend::new(100, 28);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                draw(
                    frame,
                    Theme::new(ColorCapability::None),
                    &title(),
                    None,
                    &state,
                    &model,
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
    fn stale_input_is_explicit() {
        let old = Instant::now() - DEFAULT_FRESHNESS_TTL - Duration::from_secs(1);
        let telemetry = TelemetrySnapshot {
            joypad: Some(Timestamped {
                received_at: old,
                value: JoypadDevicesSample::default(),
            }),
            ..TelemetrySnapshot::default()
        };
        assert!(render_page(Page::Input, &telemetry).contains("stale tool"));
    }

    #[test]
    fn overview_renders_root_disk_uptime_and_staleness() {
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
                }],
                ..HostSample::default()
            },
        };
        let lines = host_lines(Some(&host), Instant::now())
            .into_iter()
            .map(|line| line.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(lines.contains("root"));
        assert!(lines.contains("apfs"));
        assert!(lines.contains("1m 05s"));
        assert!(lines.contains("stale"));
    }

    #[test]
    fn bus_sort_is_deterministic_for_rate_topic_and_producer() {
        let a = TopicMetric {
            topic: "z/topic".to_string(),
            from_participant: "alpha".to_string(),
            ingress_rate_hz: 1.0,
            count: 1,
        };
        let b = TopicMetric {
            topic: "a/topic".to_string(),
            from_participant: "zeta".to_string(),
            ingress_rate_hz: 5.0,
            count: 2,
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
    fn overscroll_keeps_the_oldest_available_row_visible() {
        assert_eq!(bounded_scroll_offset(usize::MAX, 3), 2);
        assert_eq!(bounded_scroll_offset(usize::MAX, 1), 0);
        assert_eq!(bounded_scroll_offset(usize::MAX, 0), 0);
    }

    #[test]
    fn input_status_comes_only_from_authoritative_tool_state() {
        let now = Instant::now();
        let sample = |value| Timestamped {
            value,
            received_at: now,
        };
        assert_eq!(input_status(None, now), "backend unavailable");
        assert_eq!(
            input_status(Some(&sample(JoypadDevicesSample::default())), now),
            "no controller"
        );
        let ready_device = JoypadDevice {
            id: "pad".to_string(),
            name: "Pad".to_string(),
            status: JoypadDeviceStatus::Ready,
        };
        assert_eq!(
            input_status(
                Some(&sample(JoypadDevicesSample {
                    available: vec![ready_device.clone()],
                    ..JoypadDevicesSample::default()
                })),
                now
            ),
            "compatible unselected"
        );
        assert_eq!(
            input_status(
                Some(&sample(JoypadDevicesSample {
                    available: vec![ready_device],
                    selected: Some("pad".to_string()),
                    ..JoypadDevicesSample::default()
                })),
                now
            ),
            "ready selected"
        );
        for (device_status, expected) in [
            (JoypadDeviceStatus::Disconnected, "disconnected"),
            (JoypadDeviceStatus::Unsupported, "unsupported"),
        ] {
            assert_eq!(
                input_status(
                    Some(&sample(JoypadDevicesSample {
                        available: vec![JoypadDevice {
                            id: "pad".to_string(),
                            name: "Pad".to_string(),
                            status: device_status,
                        }],
                        selected: Some("pad".to_string()),
                        ..JoypadDevicesSample::default()
                    })),
                    now
                ),
                expected
            );
        }
        assert_eq!(
            input_status(
                Some(&sample(JoypadDevicesSample {
                    unavailable_reason: Some("no motion limits".to_string()),
                    ..JoypadDevicesSample::default()
                })),
                now
            ),
            "backend unavailable"
        );
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
        terminal
            .draw(|frame| {
                draw(
                    frame,
                    Theme::new(ColorCapability::None),
                    &title(),
                    None,
                    &AppState::default(),
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
