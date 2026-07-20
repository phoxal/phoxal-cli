//! Logs rendering responsibilities.

use super::*;

pub(super) fn draw_logs(
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
                    .is_none_or(|anchor| line.event_time <= anchor)
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
