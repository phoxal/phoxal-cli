mod model;

pub use model::{BusModel, BusSort};

use tuirealm::ratatui::Frame;
use tuirealm::ratatui::layout::{Constraint, Direction, Layout, Rect};
use tuirealm::ratatui::widgets::{Paragraph, Row, Table};

use crate::Theme;
use crate::app::{AppModel, BusPanelId, PanelId};
use crate::components::shared::panel_block;

pub fn render(frame: &mut Frame, area: Rect, model: &AppModel, theme: Theme) {
    let [controls, topics] = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(4), Constraint::Min(4)])
        .areas(area);
    frame.render_widget(
        Paragraph::new(format!(
            "{}filter:{}   {}sort:{:?}   {}internal:{}",
            marker(model.bus.control_candidate, 0),
            if model.bus.filter.is_empty() {
                "*"
            } else {
                &model.bus.filter
            },
            marker(model.bus.control_candidate, 1),
            model.bus.sort,
            marker(model.bus.control_candidate, 2),
            if model.bus.show_internal {
                "show"
            } else {
                "hide"
            },
        ))
        .block(panel_block(
            model,
            PanelId::Bus(BusPanelId::Controls),
            "Controls",
            theme,
        )),
        controls,
    );
    let rows = model
        .bus
        .rows
        .iter()
        .filter(|row| model.bus.show_internal || !row.topic.contains("/_"))
        .skip(model.bus.scroll)
        .map(|row| {
            Row::new(vec![
                row.topic.clone(),
                row.participant.clone(),
                format!("{:.1}", row.rate_hz),
                row.count.to_string(),
            ])
        });
    frame.render_widget(
        Table::new(
            rows,
            [
                Constraint::Percentage(45),
                Constraint::Percentage(30),
                Constraint::Length(9),
                Constraint::Length(10),
            ],
        )
        .header(Row::new(["topic", "producer", "msg/s", "count"]))
        .block(panel_block(
            model,
            PanelId::Bus(BusPanelId::Topics),
            "Topics",
            theme,
        )),
        topics,
    );
}

fn marker(candidate: usize, index: usize) -> &'static str {
    if candidate == index { ">" } else { " " }
}
