//! Overview rendering responsibilities.

use super::*;

pub(super) fn draw_overview(
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
            lines.push(runtime_attention_line(theme, status));
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

pub(super) fn runtime_attention_line(theme: Theme, status: &ParticipantStatus) -> Line<'static> {
    let restarts = status.restart_count;
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
