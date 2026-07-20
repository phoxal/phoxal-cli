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
    frame.render_widget(
        Paragraph::new(Line::styled(
            format!(
                "    {:<id_width$} {:<state_width$} {:>restart_width$}",
                "ID",
                "STATE",
                if columns.compact { "RST" } else { "RESTARTS" },
                id_width = columns.id,
                state_width = columns.state,
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
pub(super) struct RuntimeColumns {
    id: usize,
    state: usize,
    restarts: usize,
    compact: bool,
}

pub(super) fn runtime_columns(inner_width: u16) -> RuntimeColumns {
    if inner_width >= 68 {
        return RuntimeColumns {
            id: 26,
            state: 11,
            restarts: 8,
            compact: false,
        };
    }
    let state = 7;
    let restarts = 5;
    let fixed = 4 + 2 + state + restarts;
    RuntimeColumns {
        id: usize::from(inner_width).saturating_sub(fixed).max(6),
        state,
        restarts,
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
    format!(
        "{} {id} {state} {restarts:>restart_width$}",
        state_symbol(status.state),
        restart_width = columns.restarts,
    )
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

pub(super) fn draw_contracts(
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
