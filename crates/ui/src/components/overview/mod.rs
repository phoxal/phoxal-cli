mod model;

pub use model::OverviewModel;

use tuirealm::ratatui::Frame;
use tuirealm::ratatui::layout::{Constraint, Direction, Layout, Rect};
use tuirealm::ratatui::text::{Line, Text};
use tuirealm::ratatui::widgets::Paragraph;

use crate::Theme;
use crate::app::AppModel;

pub fn render(frame: &mut Frame, area: Rect, model: &AppModel, theme: Theme) {
    let [summary, diagnostics] = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(65), Constraint::Percentage(35)])
        .areas(area);
    let [status, health] = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
        .areas(summary);
    let status_text = model.overview.supervisor.as_ref().map_or_else(
        || Text::from("Waiting for attachment snapshot"),
        |snapshot| {
            Text::from(vec![
                Line::from(format!("Project: {}", sanitize(&snapshot.project))),
                Line::from(format!(
                    "Connection: {}",
                    connection_label(model.overview.connection.as_ref())
                )),
                Line::from(format!("Lifecycle: {:?}", snapshot.lifecycle)),
                Line::from(format!("Robot: {}", sanitize(snapshot.robot.as_str()))),
                Line::from(format!("Execution: {}", snapshot.execution)),
                Line::from(format!("Processes: {}", model.overview.processes.len())),
            ])
        },
    );
    frame.render_widget(
        Paragraph::new(status_text).block(crate::components::shared::outer_panel_block(
            " Project ",
            theme,
        )),
        status,
    );
    let source_states = model
        .overview
        .source_health
        .as_ref()
        .into_iter()
        .flat_map(|health| health.sources.iter())
        .map(|(source, status)| format!("{}: {status:?}", source.label()))
        .collect::<Vec<_>>();
    let ingress_dropped = model
        .overview
        .source_health
        .as_ref()
        .map_or(0, |health| health.ingress_dropped);
    let health_text = if source_states.is_empty() {
        format!("All observed sources are fresh\nEpoch history shed: {ingress_dropped}")
    } else {
        format!(
            "Epoch history shed: {ingress_dropped}\n{}",
            source_states.join("\n")
        )
    };
    frame.render_widget(
        Paragraph::new(health_text)
            .style(crate::theme::role::fg(theme, crate::Role::Steel))
            .block(crate::components::shared::outer_panel_block(
                " Health ", theme,
            )),
        health,
    );
    let diagnostic_text = model
        .overview
        .diagnostics
        .iter()
        .rev()
        .take(diagnostics.height.saturating_sub(2) as usize)
        .rev()
        .map(|message| sanitize(message))
        .collect::<Vec<_>>()
        .join("\n");
    frame.render_widget(
        Paragraph::new(if diagnostic_text.is_empty() {
            "No diagnostics".to_string()
        } else {
            diagnostic_text
        })
        .block(crate::components::shared::outer_panel_block(
            " Diagnostics ",
            theme,
        )),
        diagnostics,
    );
}

fn connection_label(connection: Option<&phoxal_cli_observation::ConnectionObservation>) -> String {
    use phoxal_cli_observation::ConnectionObservation;
    match connection {
        None => "waiting".to_string(),
        Some(ConnectionObservation::Connected) => "connected".to_string(),
        Some(ConnectionObservation::Lost { reason }) => {
            format!("lost: {}", sanitize(reason))
        }
    }
}

fn sanitize(value: &str) -> String {
    crate::format::sanitize_terminal_text(value)
}
