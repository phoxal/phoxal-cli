use tuirealm::ratatui::Frame;
use tuirealm::ratatui::layout::Rect;
use tuirealm::ratatui::text::{Line, Span};
use tuirealm::ratatui::widgets::{Block, Borders, Paragraph};

use crate::app::{AppModel, FocusRoute, PageId};
use crate::{Role, Theme};

pub fn render_header(frame: &mut Frame, area: Rect, model: &AppModel, theme: Theme) {
    let (project, lifecycle) = model.overview.supervisor.as_ref().map_or(
        ("waiting for resident".to_string(), "connecting".to_string()),
        |snapshot| {
            (
                phoxal_cli_core::session::sanitize_terminal_text(&snapshot.project),
                format!("{:?}", snapshot.lifecycle).to_lowercase(),
            )
        },
    );
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                " phoxal ",
                crate::theme::role::selected(theme, Role::Accent),
            ),
            Span::raw(project),
            Span::raw("  "),
            Span::styled(lifecycle, crate::theme::role::fg(theme, Role::Steel)),
        ]))
        .block(Block::default().borders(Borders::BOTTOM)),
        area,
    );
}

pub fn render_tabs(frame: &mut Frame, area: Rect, model: &AppModel, theme: Theme) {
    let page = model.route.page();
    let tab_candidate = match model.route {
        FocusRoute::Tabs { candidate, .. } => Some(candidate),
        _ => None,
    };
    let tabs = PageId::ALL
        .iter()
        .enumerate()
        .flat_map(|(index, candidate)| {
            let selected = *candidate == page;
            let focused = tab_candidate == Some(*candidate);
            let marker = if focused {
                ">"
            } else if selected {
                "*"
            } else {
                " "
            };
            [
                Span::styled(
                    format!("{marker}{} {}", index + 1, candidate.label()),
                    if focused {
                        crate::theme::role::selected(theme, Role::Accent)
                    } else if selected {
                        crate::theme::role::fg(theme, Role::Accent)
                    } else {
                        crate::theme::role::muted(theme)
                    },
                ),
                Span::raw("  "),
            ]
        })
        .collect::<Vec<_>>();
    frame.render_widget(Paragraph::new(Line::from(tabs)), area);
}

pub fn render_footer(frame: &mut Frame, area: Rect, model: &AppModel, theme: Theme) {
    let depth = match model.route {
        FocusRoute::Tabs { .. } => "tabs",
        FocusRoute::Panels { .. } => "panels",
        FocusRoute::Content { .. } => "content",
        FocusRoute::Modal { .. } => "modal",
    };
    let diagnostic = model
        .overview
        .diagnostics
        .last()
        .map_or_else(String::new, |message| {
            format!(
                "  ! {}",
                phoxal_cli_core::session::sanitize_terminal_text(message)
            )
        });
    frame.render_widget(
        Paragraph::new(format!(
            " {depth}  Enter descend  Esc ascend  ? help  i session  S stop  q detach{diagnostic}"
        ))
        .style(crate::theme::role::muted(theme))
        .block(Block::default().borders(Borders::TOP)),
        area,
    );
}

#[cfg(test)]
mod tests {
    use tuirealm::ratatui::Terminal;
    use tuirealm::ratatui::backend::TestBackend;

    use super::*;
    use crate::ColorCapability;

    #[test]
    fn tab_focus_marker_moves_without_hiding_the_active_page() {
        let mut terminal = Terminal::new(TestBackend::new(100, 2)).expect("test terminal");
        let model = AppModel {
            route: FocusRoute::Tabs {
                page: PageId::Overview,
                candidate: PageId::Runtimes,
            },
            ..AppModel::default()
        };
        terminal
            .draw(|frame| {
                render_tabs(
                    frame,
                    frame.area(),
                    &model,
                    Theme::new(ColorCapability::None),
                );
            })
            .expect("render tabs");
        let contents = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(contents.contains("*1 Overview"));
        assert!(contents.contains(">2 Runtimes"));
    }
}
