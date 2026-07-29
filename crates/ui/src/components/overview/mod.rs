mod model;

pub use model::OverviewModel;

use tuirealm::ratatui::Frame;
use tuirealm::ratatui::layout::{Constraint, Direction, Layout, Rect};
use tuirealm::ratatui::widgets::{Block, Borders, Paragraph};

use crate::Theme;
use crate::app::AppModel;

pub fn render(frame: &mut Frame, area: Rect, model: &AppModel, theme: Theme) {
    let [status, health] = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
        .areas(area);
    let status_text = model.overview.supervisor.as_ref().map_or_else(
        || "Waiting for attachment snapshot".to_string(),
        |snapshot| {
            format!(
                "Project: {}\nLifecycle: {:?}\nFramework: {}\nRouter: {}\nProcesses: {}",
                snapshot.project,
                snapshot.lifecycle,
                snapshot.framework_train,
                snapshot.router,
                model.overview.processes.len()
            )
        },
    );
    frame.render_widget(
        Paragraph::new(status_text)
            .block(Block::default().title(" Project ").borders(Borders::ALL)),
        status,
    );
    let stale = model
        .overview
        .freshness
        .iter()
        .filter(|(_, freshness)| matches!(freshness, phoxal_cli_observation::Freshness::Stale))
        .map(|(source, _)| source.as_str())
        .collect::<Vec<_>>();
    let health_text = if stale.is_empty() {
        "All observed sources are fresh".to_string()
    } else {
        format!("Stale sources:\n{}", stale.join("\n"))
    };
    frame.render_widget(
        Paragraph::new(health_text)
            .style(crate::theme::role::fg(theme, crate::Role::Steel))
            .block(Block::default().title(" Health ").borders(Borders::ALL)),
        health,
    );
}
