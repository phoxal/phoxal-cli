mod model;

pub use model::RuntimesModel;

use tuirealm::ratatui::Frame;
use tuirealm::ratatui::layout::{Constraint, Direction, Layout, Rect};
use tuirealm::ratatui::text::{Line, Span};
use tuirealm::ratatui::widgets::{Paragraph, Row, Table};

use phoxal_cli_observation::ProcessObservation;

use crate::Theme;
use crate::app::{AppModel, PanelId, RuntimesPanelId};
use crate::components::shared::panel_block;

/// What this client knows about the runtime it started itself.
///
/// The snapshot column says present or absent and can say nothing more - the
/// supervisor watches leases and never started anything. This column is the
/// other half, and it is empty for an attachment to somebody else's execution
/// because that client has no honest answer to put there.
fn local_label(process: &ProcessObservation) -> String {
    process
        .local
        .as_ref()
        .map(|local| crate::format::sanitize_terminal_text(&local.state.label()))
        .unwrap_or_default()
}

pub fn render(frame: &mut Frame, area: Rect, model: &AppModel, theme: Theme) {
    let [processes, performance] = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(42), Constraint::Percentage(58)])
        .areas(area);
    let rows = model.overview.processes.iter().map(|(key, process)| {
        let candidate = model.runtimes.candidate.as_ref() == Some(key);
        Row::new(vec![
            if candidate { ">" } else { " " }.to_string(),
            key.to_string(),
            format!("{:?}", process.row.state).to_lowercase(),
            local_label(process),
        ])
    });
    frame.render_widget(
        Table::new(
            rows,
            [
                Constraint::Length(1),
                Constraint::Min(18),
                Constraint::Length(8),
                Constraint::Length(16),
            ],
        )
        .header(Row::new(["", "process", "state", "this session"]))
        .block(panel_block(
            model,
            PanelId::Runtimes(RuntimesPanelId::Processes),
            "Processes",
            theme,
        )),
        processes,
    );
    let lines = model
        .runtimes
        .rows
        .iter()
        .skip(model.runtimes.scroll)
        .take((performance.height.saturating_sub(2) as usize).div_ceil(2))
        .flat_map(|row| {
            let summary = row.sample.summary();
            [
                Line::from(vec![
                    Span::raw(format!("{}  ", row.sample.participant_id())),
                    Span::raw(format!("{:.1} msg/s", summary.message_rate_hz)),
                ]),
                Line::from(format!(
                    "  drops {}  decode {}  evictions {}",
                    summary.drops, summary.decode_errors, row.capacity_evictions
                )),
            ]
        })
        .collect::<Vec<_>>();
    frame.render_widget(
        Paragraph::new(lines).block(panel_block(
            model,
            PanelId::Runtimes(RuntimesPanelId::Performance),
            "Performance",
            theme,
        )),
        performance,
    );
}
