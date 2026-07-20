//! Chrome rendering responsibilities.

use super::*;

pub(super) fn draw_too_small(
    frame: &mut Frame,
    theme: Theme,
    state: &AppState,
    area: Rect,
) -> bool {
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

pub(super) fn startup_phase_lines(phase: &PhaseRow) -> Vec<Line<'static>> {
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

pub(super) fn draw_header(
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

pub(super) fn compact_header_status(
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

pub(super) fn header_identity_lines(
    theme: Theme,
    title: &TitleInfo,
    width: u16,
) -> Vec<Line<'static>> {
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

pub(super) fn header_host_lines(
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

pub(super) fn header_clock_lines(
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

pub(super) fn draw_tabs(frame: &mut Frame, theme: Theme, state: &AppState, area: Rect) {
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
