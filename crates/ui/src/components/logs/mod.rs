mod model;

pub use model::{Editing, LogSourceFilter, LogsModel, SeverityFilter};

use std::time::UNIX_EPOCH;

use tuirealm::ratatui::Frame;
use tuirealm::ratatui::layout::{Constraint, Direction, Layout, Rect};
use tuirealm::ratatui::text::{Line, Span};
use tuirealm::ratatui::widgets::Paragraph;

use crate::Theme;
use crate::app::{AppModel, LogsPanelId, PanelId};
use crate::components::shared::panel_block;

pub fn render(frame: &mut Frame, area: Rect, model: &AppModel, theme: Theme) {
    let [filters, stream] = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(4), Constraint::Min(4)])
        .areas(area);
    let controls = [
        format!("source:{:?}", model.logs.source),
        format!("participant:{}", empty_as_star(&model.logs.participant)),
        format!("severity:{:?}", model.logs.severity),
        format!("contains:{}", empty_as_star(&model.logs.text)),
        format!("follow:{}", if model.logs.follow { "on" } else { "off" }),
    ];
    let spans = controls
        .iter()
        .enumerate()
        .flat_map(|(index, control)| {
            [
                Span::styled(
                    if index == model.logs.control_candidate {
                        format!(">{control}")
                    } else {
                        format!(" {control}")
                    },
                    if index == model.logs.control_candidate {
                        crate::ratatui::candidate(theme, crate::Role::Accent)
                    } else {
                        crate::ratatui::muted(theme)
                    },
                ),
                Span::raw("  "),
            ]
        })
        .collect::<Vec<_>>();
    frame.render_widget(
        Paragraph::new(Line::from(spans)).block(panel_block(
            model,
            PanelId::Logs(LogsPanelId::Filters),
            "Filters",
            theme,
        )),
        filters,
    );
    let height = stream.height.saturating_sub(2) as usize;
    let start = model
        .logs
        .rows
        .len()
        .saturating_sub(height.saturating_add(model.logs.scroll));
    let lines = model
        .logs
        .rows
        .iter()
        .skip(start)
        .take(height)
        .map(|row| {
            let seconds = row
                .event_time
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            Line::from(format!(
                "{seconds:>10} {:<5} {:<20} {}",
                format!("{:?}", row.severity).to_lowercase(),
                row.participant,
                phoxal_cli_core::session::sanitize_terminal_text(&row.text)
            ))
        })
        .collect::<Vec<_>>();
    frame.render_widget(
        Paragraph::new(lines).block(panel_block(
            model,
            PanelId::Logs(LogsPanelId::Stream),
            "Stream",
            theme,
        )),
        stream,
    );
}

fn empty_as_star(value: &str) -> &str {
    if value.is_empty() { "*" } else { value }
}
