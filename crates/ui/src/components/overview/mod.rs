mod model;

pub use model::OverviewModel;

use tuirealm::ratatui::Frame;
use tuirealm::ratatui::layout::{Constraint, Direction, Layout, Rect};
use tuirealm::ratatui::widgets::{Block, Borders, Paragraph};

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
        || "Waiting for attachment snapshot".to_string(),
        |snapshot| {
            let mode = snapshot.simulation.as_ref().map_or_else(
                || "native".to_string(),
                |simulation| {
                    format!(
                        "simulation {} / {}",
                        sanitize(&simulation.profile),
                        sanitize(&simulation.world)
                    )
                },
            );
            let startup = snapshot
                .startup
                .steps
                .iter()
                .find(|step| {
                    step.state == phoxal_cli_core::runtime::StartupStepState::Active
                })
                .map_or_else(
                    || {
                        format!(
                            "{} steps complete",
                            snapshot
                                .startup
                                .steps
                                .iter()
                                .filter(|step| {
                                    step.state
                                        == phoxal_cli_core::runtime::StartupStepState::Done
                                })
                                .count()
                        )
                    },
                    |step| {
                        let label = startup_step_label(step.kind);
                        step.detail.as_deref().map_or_else(
                            || format!("active: {label}"),
                            |detail| format!("active: {label} · {}", sanitize(detail)),
                        )
                    },
                );
            format!(
                "Project: {}\nMode: {mode}\nConnection: {}\nLifecycle: {:?}\nStartup: {startup}\nFramework: {}\nRouter: {}\nProcesses: {}",
                sanitize(&snapshot.project),
                connection_label(model.overview.connection.as_ref()),
                snapshot.lifecycle,
                sanitize(&snapshot.framework_train),
                sanitize(&snapshot.router),
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
    let source_states = model
        .overview
        .source_health
        .as_ref()
        .into_iter()
        .flat_map(|health| health.sources.iter())
        .map(|(source, status)| format!("{}: {status:?}", sanitize(source)))
        .collect::<Vec<_>>();
    let ingress_dropped = model
        .overview
        .source_health
        .as_ref()
        .map_or(0, |health| health.ingress_dropped);
    let devices = model
        .overview
        .devices
        .as_ref()
        .map_or(0, |devices| devices.robots.len());
    let health_text = if stale.is_empty() && source_states.is_empty() {
        format!(
            "All observed sources are fresh\nRobot devices: {devices}\nEpoch history shed: {ingress_dropped}"
        )
    } else {
        format!(
            "Stale: {}\nRobot devices: {devices}\nEpoch history shed: {ingress_dropped}\n{}",
            stale.join(", "),
            source_states.join("\n")
        )
    };
    frame.render_widget(
        Paragraph::new(health_text)
            .style(crate::theme::role::fg(theme, crate::Role::Steel))
            .block(Block::default().title(" Health ").borders(Borders::ALL)),
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
        .block(
            Block::default()
                .title(" Diagnostics ")
                .borders(Borders::ALL),
        ),
        diagnostics,
    );
}

fn startup_step_label(kind: phoxal_cli_core::runtime::StartupStepKind) -> &'static str {
    use phoxal_cli_core::runtime::StartupStepKind;
    match kind {
        StartupStepKind::Project => "Project",
        StartupStepKind::PrepareRuntime => "Prepare runtime",
        StartupStepKind::Infrastructure => "Infrastructure",
        StartupStepKind::Graph => "Robot graph",
    }
}

fn connection_label(connection: Option<&phoxal_cli_observation::ConnectionObservation>) -> String {
    use phoxal_cli_observation::ConnectionObservation;
    match connection {
        None => "waiting".to_string(),
        Some(ConnectionObservation::Connected) => "connected".to_string(),
        Some(ConnectionObservation::Reconnecting { attempt }) => {
            format!("reconnecting (attempt {attempt})")
        }
        Some(ConnectionObservation::Terminal) => "terminal".to_string(),
        Some(ConnectionObservation::Lost { reason }) => {
            format!("lost: {}", sanitize(reason))
        }
    }
}

fn sanitize(value: &str) -> String {
    crate::format::sanitize_terminal_text(value)
}
