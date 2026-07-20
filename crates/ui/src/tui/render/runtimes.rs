//! Runtimes rendering responsibilities.

use super::*;

pub(super) fn draw_runtimes(
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

pub(super) fn runtime_section_heights(counts: [usize; 3], total_height: u16) -> [u16; 3] {
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

pub(super) fn draw_runtime_group(
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
    let header = if columns.compact {
        format!(
            "    {:<id_width$} {:<state_width$} {:<perf_width$} {:>restart_width$}",
            "ID",
            "STATE",
            "PERF",
            "RST",
            id_width = columns.id,
            state_width = columns.state,
            perf_width = columns.perf,
            restart_width = columns.restarts,
        )
    } else {
        format!(
            "    {:<id_width$} {:<state_width$} {:>rate_width$} {:>budget_width$} {:>headroom_width$} {:>pressure_width$} {:>restart_width$}",
            "ID",
            "STATE",
            "RATE",
            "BUDGET",
            "HEADROOM",
            "PRESS",
            "RST",
            id_width = columns.id,
            state_width = columns.state,
            rate_width = columns.rate,
            budget_width = columns.budget,
            headroom_width = columns.headroom,
            pressure_width = columns.pressure,
            restart_width = columns.restarts,
        )
    };
    frame.render_widget(
        Paragraph::new(Line::styled(header, color::muted(theme))),
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
pub(super) struct RuntimeColumns {
    id: usize,
    state: usize,
    restarts: usize,
    perf: usize,
    rate: usize,
    budget: usize,
    headroom: usize,
    pressure: usize,
    compact: bool,
}

pub(super) fn runtime_columns(inner_width: u16) -> RuntimeColumns {
    if inner_width >= 92 {
        return RuntimeColumns {
            id: 22,
            state: 10,
            restarts: 4,
            perf: 0,
            rate: 9,
            budget: 8,
            headroom: 9,
            pressure: 8,
            compact: false,
        };
    }
    let state = 7;
    let restarts = 5;
    let perf = usize::from(inner_width)
        .saturating_sub(4 + 3 + state + restarts + 6)
        .clamp(8, 20);
    let fixed = 4 + 3 + state + perf + restarts;
    RuntimeColumns {
        id: usize::from(inner_width).saturating_sub(fixed).max(6),
        state,
        restarts,
        perf,
        rate: 0,
        budget: 0,
        headroom: 0,
        pressure: 0,
        compact: true,
    }
}

pub(super) fn runtime_row(
    status: &ParticipantStatus,
    model: &SessionViewModel<'_>,
    columns: RuntimeColumns,
) -> String {
    let observation = model.runtime.observation(&status.id);
    let restarts = observation.map_or(status.restart_count, |observation| {
        observation.displayed_restarts()
    });
    let id = sanitize_and_fit_cell(&status.id, columns.id);
    let state = fit_cell(status.state.label(), columns.state);
    let performance = runtime_performance(status, model);
    if columns.compact {
        let perf = fit_cell(&performance.compact, columns.perf);
        return format!(
            "{} {id} {state} {perf} {restarts:>restart_width$}",
            state_symbol(status.state),
            restart_width = columns.restarts,
        );
    }
    format!(
        "{} {id} {state} {rate:>rate_width$} {budget:>budget_width$} {headroom:>headroom_width$} {pressure:>pressure_width$} {restarts:>restart_width$}",
        state_symbol(status.state),
        rate = performance.rate,
        budget = performance.budget,
        headroom = performance.headroom,
        pressure = performance.pressure,
        rate_width = columns.rate,
        budget_width = columns.budget,
        headroom_width = columns.headroom,
        pressure_width = columns.pressure,
        restart_width = columns.restarts,
    )
}

struct RuntimePerformanceCells {
    compact: String,
    rate: String,
    budget: String,
    headroom: String,
    pressure: String,
}

fn runtime_performance(
    status: &ParticipantStatus,
    model: &SessionViewModel<'_>,
) -> RuntimePerformanceCells {
    let absent = match status.present {
        Some(false) => Some("missing"),
        Some(true) => model
            .telemetry
            .runtime(&status.id)
            .filter(|sample| sample.is_stale(model.now, DEFAULT_FRESHNESS_TTL))
            .map(|_| "stalled"),
        None => None,
    };
    let Some(sample) = model.telemetry.runtime(&status.id) else {
        let label = absent.unwrap_or("waiting").to_string();
        return RuntimePerformanceCells {
            compact: label.clone(),
            rate: label,
            budget: "-".to_string(),
            headroom: "-".to_string(),
            pressure: "-".to_string(),
        };
    };
    if let Some(label) = absent {
        return RuntimePerformanceCells {
            compact: label.to_string(),
            rate: label.to_string(),
            budget: "-".to_string(),
            headroom: "-".to_string(),
            pressure: "-".to_string(),
        };
    }
    let summary = sample.value.summary();
    let rate = format!("{:.1}/s", summary.message_rate_hz);
    let budget = summary
        .budget_utilization_pct
        .map_or_else(|| "n/a".to_string(), |value| format!("{value:.0}%"));
    let headroom = summary
        .headroom_ns
        .map_or_else(|| "n/a".to_string(), format_signed_duration_ns);
    let pressure = summary
        .current_pressure_pct
        .map_or_else(|| "n/a".to_string(), |value| format!("{value:.0}%"));
    RuntimePerformanceCells {
        compact: format!("{rate} · {budget} · {pressure}"),
        rate,
        budget,
        headroom,
        pressure,
    }
}

fn format_signed_duration_ns(value: i64) -> String {
    let sign = if value < 0 { "-" } else { "+" };
    let magnitude = value.unsigned_abs();
    if magnitude >= 1_000_000 {
        format!("{sign}{:.1}ms", magnitude as f64 / 1_000_000.0)
    } else if magnitude >= 1_000 {
        format!("{sign}{:.1}us", magnitude as f64 / 1_000.0)
    } else {
        format!("{sign}{magnitude}ns")
    }
}

pub(super) fn draw_runtime_detail(
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
        Line::from(format!("restarts      {restarts}")),
    ];
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(8),
            Constraint::Length(6),
            Constraint::Min(5),
        ])
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
    draw_runtime_performance(frame, theme, status, model, vertical[1]);
    draw_runtime_topics(frame, theme, state, status, model, vertical[2]);
}

fn draw_runtime_performance(
    frame: &mut Frame,
    theme: Theme,
    status: &ParticipantStatus,
    model: &SessionViewModel<'_>,
    area: Rect,
) {
    let Some(sample) = model.telemetry.runtime(&status.id) else {
        frame.render_widget(
            Paragraph::new("Waiting for portable runtime telemetry")
                .block(shell_block(theme, "Performance")),
            area,
        );
        return;
    };
    let summary = sample.value.summary();
    let step_rate = summary.step_rate_hz.map_or_else(
        || "event-driven".to_string(),
        |value| format!("{value:.1}/s"),
    );
    let cells = runtime_performance(status, model);
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(format!(
                "step rate     {step_rate:<14} message rate  {}",
                cells.rate
            )),
            Line::from(format!(
                "budget        {:<14} headroom      {}",
                cells.budget, cells.headroom
            )),
            Line::from(format!(
                "pressure      {:<14} high-water    {} · drops {} · decode {}",
                cells.pressure,
                summary
                    .high_water_pressure_pct
                    .map_or_else(|| "n/a".to_string(), |value| format!("{value:.0}%")),
                summary.drops,
                summary.decode_errors,
            )),
        ])
        .block(shell_block(theme, "Portable performance")),
        area,
    );
}

fn draw_runtime_topics(
    frame: &mut Frame,
    theme: Theme,
    state: &AppState,
    status: &ParticipantStatus,
    model: &SessionViewModel<'_>,
    area: Rect,
) {
    let Some(sample) = model.telemetry.runtime(&status.id) else {
        frame.render_widget(
            Paragraph::new("No topic rows yet").block(shell_block(theme, "Topics")),
            area,
        );
        return;
    };
    let all_rows = sample
        .value
        .topics
        .iter()
        .chain(sample.value.overflow.iter())
        .collect::<Vec<_>>();
    let visible = usize::from(area.height.saturating_sub(3)).max(1);
    let start = state
        .runtime_topic_offset
        .min(all_rows.len().saturating_sub(visible));
    let end = start.saturating_add(visible).min(all_rows.len());
    let title = if all_rows.len() > visible {
        format!(
            "Topics · {}-{} of {} · Up/Down scroll",
            start + 1,
            end,
            all_rows.len()
        )
    } else {
        format!("Topics · {}", all_rows.len())
    };
    let block = shell_block(theme, &title);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(0)])
        .split(inner);
    frame.render_widget(
        Paragraph::new(Line::styled(
            format!(
                "{:<28} {:<3} {:<4} {:>8} {:>9} {:>9} {:>8}",
                "TOPIC", "DIR", "BUF", "RATE", "DEPTH", "HIGH", "LOSS"
            ),
            color::muted(theme),
        )),
        rows[0],
    );
    let lines = all_rows[start..end]
        .iter()
        .map(|row| {
            let topic = sanitize_and_fit_cell(&row.topic, 28);
            let direction = match row.direction { RuntimeDirection::Publish => "pub", RuntimeDirection::Subscribe => "sub", RuntimeDirection::Mixed => "mix" };
            let buffer = match row.buffer_kind { RuntimeBufferKind::Outbound => "out", RuntimeBufferKind::Latest => "last", RuntimeBufferKind::Subscriber => "sub", RuntimeBufferKind::Mixed => "mix" };
            let loss = row.drops.saturating_add(row.latest_overwrites).saturating_add(row.bounded_evictions).saturating_add(row.decode_errors);
            Line::from(format!("{topic:<28} {direction:<3} {buffer:<4} {:>7.1}/s {:>4}/{:<4} {:>4}/{:<4} {loss:>8}", row.rate_hz, row.current_depth, row.capacity, row.high_water_depth, row.capacity))
        })
        .collect::<Vec<_>>();
    frame.render_widget(Paragraph::new(lines), rows[1]);
}
