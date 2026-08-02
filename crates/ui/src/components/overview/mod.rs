mod model;

pub use model::OverviewModel;

use tuirealm::ratatui::Frame;
use tuirealm::ratatui::layout::{Constraint, Direction, Layout, Rect};
use tuirealm::ratatui::text::{Line, Span, Text};
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
            let timeline = startup_timeline(
                &snapshot.startup,
                status.width.saturating_sub(2) as usize,
                status.height.saturating_sub(2) as usize,
                theme.supports_unicode(),
            );
            let mut lines = vec![
                Line::from(format!("Project: {}", sanitize(&snapshot.project))),
                Line::from(format!("Mode: {mode}")),
                Line::from(format!(
                    "Connection: {}",
                    connection_label(model.overview.connection.as_ref())
                )),
                Line::from(format!("Lifecycle: {:?}", snapshot.lifecycle)),
            ];
            for (index, timeline_line) in timeline.into_iter().enumerate() {
                let prefix = if index == 0 { "Startup: " } else { "         " };
                lines.push(Line::from(vec![
                    Span::raw(prefix),
                    Span::styled(
                        timeline_line.text,
                        startup_step_style(theme, timeline_line.state),
                    ),
                ]));
            }
            lines.extend([
                Line::from(format!(
                    "Framework: {}",
                    sanitize(&snapshot.framework_train)
                )),
                Line::from(format!("Router: {}", sanitize(&snapshot.router))),
                Line::from(format!("Processes: {}", model.overview.processes.len())),
            ]);
            Text::from(lines)
        },
    );
    frame.render_widget(
        Paragraph::new(status_text).block(crate::components::shared::outer_panel_block(
            " Project ",
            theme,
        )),
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
    let health_text = if stale.is_empty() && source_states.is_empty() {
        format!("All observed sources are fresh\nEpoch history shed: {ingress_dropped}")
    } else {
        format!(
            "Stale: {}\nEpoch history shed: {ingress_dropped}\n{}",
            stale.join(", "),
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

fn startup_step_label(kind: phoxal_cli_core::runtime::StartupStepKind) -> &'static str {
    use phoxal_cli_core::runtime::StartupStepKind;
    match kind {
        StartupStepKind::Project => "Project",
        StartupStepKind::PrepareRuntime => "Prepare runtime",
        StartupStepKind::Infrastructure => "Infrastructure",
        StartupStepKind::Graph => "Robot graph",
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct StartupTimelineLine {
    text: String,
    state: Option<phoxal_cli_core::runtime::StartupStepState>,
}

fn startup_step_style(
    theme: Theme,
    state: Option<phoxal_cli_core::runtime::StartupStepState>,
) -> tuirealm::ratatui::style::Style {
    use phoxal_cli_core::runtime::StartupStepState;
    let role = match state {
        Some(StartupStepState::Done) => crate::Role::Success,
        Some(StartupStepState::Active) => crate::Role::Accent,
        Some(StartupStepState::Failed) => crate::Role::Error,
        Some(StartupStepState::Pending) | None => crate::Role::Muted,
    };
    crate::theme::role::fg(theme, role)
}

fn startup_marker(
    state: phoxal_cli_core::runtime::StartupStepState,
    unicode: bool,
) -> &'static str {
    use phoxal_cli_core::runtime::StartupStepState;
    match (unicode, state) {
        (true, StartupStepState::Done) => "✓",
        (true, StartupStepState::Active) => "◐",
        (true, StartupStepState::Failed) => "✗",
        (true, StartupStepState::Pending) => "○",
        (false, StartupStepState::Done) => "[x]",
        (false, StartupStepState::Active) => "[>]",
        (false, StartupStepState::Failed) => "[!]",
        (false, StartupStepState::Pending) => "[ ]",
    }
}

fn startup_timeline(
    startup: &phoxal_cli_core::runtime::StartupStatus,
    width: usize,
    height: usize,
    unicode: bool,
) -> Vec<StartupTimelineLine> {
    use phoxal_cli_core::runtime::StartupStepState;
    // The overview panel has room for all four typed steps only on a normal
    // terminal. Preserve the compact active-step behaviour on short/narrow
    // displays so startup context never evicts controls or diagnostics.
    if height < 11 {
        let line = startup
            .steps
            .iter()
            .find(|step| step.state == StartupStepState::Active)
            .map_or_else(
                || StartupTimelineLine {
                    text: format!(
                        "{} steps complete",
                        startup
                            .steps
                            .iter()
                            .filter(|step| step.state == StartupStepState::Done)
                            .count()
                    ),
                    state: None,
                },
                |step| StartupTimelineLine {
                    text: step.detail.as_deref().map_or_else(
                        || format!("active: {}", startup_step_label(step.kind)),
                        |detail| {
                            format!(
                                "active: {} · {}",
                                startup_step_label(step.kind),
                                sanitize(detail)
                            )
                        },
                    ),
                    state: Some(step.state),
                },
            );
        return vec![line];
    }
    startup
        .steps
        .iter()
        .map(|step| {
            let marker = startup_marker(step.state, unicode);
            let text_budget = width.saturating_sub(9).max(1);
            if width < 42 {
                return StartupTimelineLine {
                    text: console::truncate_str(
                        &format!("{marker} {}", startup_step_label(step.kind)),
                        text_budget,
                        "…",
                    )
                    .into_owned(),
                    state: Some(step.state),
                };
            }
            let detail = step
                .detail
                .as_deref()
                .map(sanitize)
                .unwrap_or_else(|| match step.state {
                    StartupStepState::Pending => "waiting".to_string(),
                    _ => String::new(),
                });
            let elapsed = step
                .elapsed_ms
                .map(|elapsed| format!(" · {:.1}s", elapsed as f64 / 1_000.0))
                .unwrap_or_default();
            let text = console::truncate_str(
                &format!(
                    "{marker} {:<16} {detail}{elapsed}",
                    startup_step_label(step.kind)
                ),
                text_budget,
                "…",
            )
            .into_owned();
            StartupTimelineLine {
                text,
                state: Some(step.state),
            }
        })
        .collect()
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

#[cfg(test)]
mod startup_timeline_tests {
    use super::*;
    use phoxal_cli_core::runtime::{StartupStatus, StartupStep, StartupStepKind, StartupStepState};

    fn timeline() -> StartupStatus {
        StartupStatus {
            steps: vec![
                StartupStep {
                    kind: StartupStepKind::Project,
                    state: StartupStepState::Done,
                    detail: Some("robot.yaml · framework 0.45.2".to_string()),
                    elapsed_ms: Some(125),
                },
                StartupStep {
                    kind: StartupStepKind::PrepareRuntime,
                    state: StartupStepState::Active,
                    detail: Some("Cargo artifacts: 2/3".to_string()),
                    elapsed_ms: None,
                },
                StartupStep {
                    kind: StartupStepKind::Infrastructure,
                    state: StartupStepState::Failed,
                    detail: Some("bad\nrouter\u{1b}[31m".to_string()),
                    elapsed_ms: Some(2_000),
                },
                StartupStep {
                    kind: StartupStepKind::Graph,
                    state: StartupStepState::Pending,
                    detail: None,
                    elapsed_ms: None,
                },
            ],
        }
    }

    fn rendered(startup: &StartupStatus, width: usize, height: usize, unicode: bool) -> String {
        startup_timeline(startup, width, height, unicode)
            .into_iter()
            .map(|line| line.text)
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn normal_height_renders_all_typed_states_details_and_elapsed_time() {
        let rendered = rendered(&timeline(), 100, 20, true);
        assert_eq!(rendered.lines().count(), 4);
        assert!(rendered.contains("✓ Project"), "{rendered}");
        assert!(rendered.contains("◐ Prepare runtime"), "{rendered}");
        assert!(rendered.contains("✗ Infrastructure"), "{rendered}");
        assert!(rendered.contains("○ Robot graph"), "{rendered}");
        assert!(rendered.contains("Cargo artifacts: 2/3"), "{rendered}");
        assert!(rendered.contains("2.0s"), "{rendered}");
        assert!(!rendered.contains('\u{1b}'), "{rendered:?}");
    }

    #[test]
    fn narrow_height_keeps_every_state_and_label_without_detail() {
        let rendered = rendered(&timeline(), 30, 20, true);
        assert_eq!(
            rendered,
            "✓ Project\n◐ Prepare runtime\n✗ Infrastructure\n○ Robot graph"
        );
    }

    #[test]
    fn short_height_falls_back_to_sanitized_active_summary() {
        let mut startup = timeline();
        startup.steps[1].detail = Some("compile\nstep\u{1b}[2J".to_string());
        let rendered = rendered(&startup, 100, 10, true);
        assert!(
            rendered.starts_with("active: Prepare runtime · "),
            "{rendered}"
        );
        assert!(!rendered.contains('\n'), "{rendered:?}");
        assert!(!rendered.contains('\u{1b}'), "{rendered:?}");
    }

    #[test]
    fn border_aware_inner_bounds_switch_at_the_real_panel_edge() {
        // A bordered status panel loses one cell on every edge. The timeline
        // must decide from its drawable inner rect, not the outer layout rect.
        assert_eq!(rendered(&timeline(), 40, 10, true).lines().count(), 1);
        assert_eq!(rendered(&timeline(), 40, 11, true).lines().count(), 4);
        let narrow = rendered(&timeline(), 41, 20, true);
        assert_eq!(narrow, rendered(&timeline(), 40, 20, true));
        for line in narrow.lines() {
            assert!(
                9 + console::measure_text_width(line) <= 41,
                "startup line exceeds inner panel width: {line:?}"
            );
        }
    }

    #[test]
    fn compact_and_detailed_timelines_fit_after_the_render_prefix() {
        for width in [20, 42] {
            let rendered = rendered(&timeline(), width, 20, true);
            assert!(
                rendered.contains('…'),
                "expected truncation at width {width}: {rendered}"
            );
            for line in rendered.lines() {
                assert!(
                    9 + console::measure_text_width(line) <= width,
                    "startup line exceeds inner width {width}: {line:?}"
                );
            }
        }
    }

    #[test]
    fn completed_short_timeline_reports_the_real_done_count() {
        let mut startup = timeline();
        for step in &mut startup.steps {
            step.state = StartupStepState::Done;
        }
        assert_eq!(rendered(&startup, 100, 10, true), "4 steps complete");
    }

    #[test]
    fn ascii_capability_uses_plain_state_markers_without_changing_labels() {
        let rendered = rendered(&timeline(), 30, 20, false);
        assert_eq!(
            rendered,
            "[x] Project\n[>] Prepare runtime\n[!] Infrastructure\n[ ] Robot graph"
        );
    }
}
