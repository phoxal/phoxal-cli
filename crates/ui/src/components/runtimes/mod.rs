mod model;

pub use model::RuntimesModel;

use tuirealm::ratatui::Frame;
use tuirealm::ratatui::layout::{Constraint, Direction, Layout, Rect};
use tuirealm::ratatui::text::{Line, Span};
use tuirealm::ratatui::widgets::{Paragraph, Row, Table};

use crate::Theme;
use crate::app::{AppModel, PanelId, RuntimesPanelId};
use crate::components::shared::panel_block;

pub fn render(frame: &mut Frame, area: Rect, model: &AppModel, theme: Theme) {
    let [processes, performance] = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(42), Constraint::Percentage(58)])
        .areas(area);
    let rows = model
        .overview
        .processes
        .iter()
        .filter(|(_, process)| crate::app::process_is_runtime(process))
        .map(|(key, process)| {
            let candidate = model.runtimes.candidate.as_ref() == Some(key);
            Row::new(vec![
                if candidate { ">" } else { " " }.to_string(),
                key.to_string(),
                format!("{:?}", process.state).to_lowercase(),
            ])
        });
    frame.render_widget(
        Table::new(
            rows,
            [
                Constraint::Length(1),
                Constraint::Min(18),
                Constraint::Length(11),
            ],
        )
        .header(Row::new(["", "process", "state"]))
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
                    Span::raw(format!(
                        "{}/{}::{}  ",
                        row.scope.namespace, row.scope.robot_id, row.sample.participant_id
                    )),
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
