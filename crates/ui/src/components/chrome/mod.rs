use tuirealm::ratatui::Frame;
use tuirealm::ratatui::layout::Rect;
use tuirealm::ratatui::text::{Line, Span};
use tuirealm::ratatui::widgets::{Block, Borders, Paragraph};

use crate::app::{AppModel, FocusRoute, PageId};
use crate::{Role, Theme};

pub fn render_header(frame: &mut Frame, area: Rect, model: &AppModel, theme: Theme) {
    let (project, lifecycle) = model.overview.supervisor.as_ref().map_or(
        (
            "waiting for supervisor".to_string(),
            "connecting".to_string(),
        ),
        |snapshot| {
            (
                crate::format::sanitize_terminal_text(&snapshot.project),
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
            Span::raw("  "),
            Span::styled(
                connection_label(model.overview.connection.as_ref()),
                crate::theme::role::fg(theme, Role::Steel),
            ),
        ]))
        .block(Block::default().borders(Borders::BOTTOM)),
        area,
    );
}

fn connection_label(connection: Option<&phoxal_cli_observation::ConnectionObservation>) -> String {
    use phoxal_cli_observation::ConnectionObservation;
    match connection {
        None => "connection waiting".to_string(),
        Some(ConnectionObservation::Connected) => "connected".to_string(),
        Some(ConnectionObservation::Lost { .. }) => "connection lost".to_string(),
    }
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

/// What `q` actually does here.
///
/// It is not the same key in both sessions: a detachable session leaves the
/// execution running, while a simulation session has no detach at all - this
/// client owns Webots, so leaving ends everything. An operator who reads
/// "detach" and gets a stopped robot has been lied to by the footer.
pub(crate) const fn quit_hint(detachable: bool) -> &'static str {
    if detachable { "detach" } else { "end session" }
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
            format!("  ! {}", crate::format::sanitize_terminal_text(message))
        });
    frame.render_widget(
        Paragraph::new(format!(
            " {depth}  Enter descend  Esc ascend  ? help  i session  S stop  q {}{diagnostic}",
            quit_hint(model.detachable)
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

    /// The footer must describe the key it actually has: `q` stops everything
    /// in a simulation session, and calling that "detach" is how an operator
    /// ends a run they meant to leave running.
    #[test]
    fn the_quit_hint_matches_what_q_does_in_this_session() {
        assert_eq!(quit_hint(true), "detach");
        assert_eq!(quit_hint(false), "end session");

        for (detachable, expected) in [(true, "q detach"), (false, "q end session")] {
            let mut terminal = Terminal::new(TestBackend::new(120, 2)).expect("test terminal");
            let model = AppModel {
                detachable,
                ..AppModel::default()
            };
            terminal
                .draw(|frame| {
                    render_footer(
                        frame,
                        frame.area(),
                        &model,
                        Theme::new(ColorCapability::None),
                    );
                })
                .expect("render footer");
            let contents = terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>();
            assert!(contents.contains(expected), "{contents}");
        }
    }

    #[test]
    fn header_and_footer_separators_remain_straight() {
        let mut terminal = Terminal::new(TestBackend::new(40, 4)).unwrap();
        let model = AppModel::default();
        let theme = Theme::new(ColorCapability::None);
        terminal
            .draw(|frame| {
                render_header(frame, Rect::new(0, 0, 40, 2), &model, theme);
                render_footer(frame, Rect::new(0, 2, 40, 2), &model, theme);
            })
            .unwrap();
        let contents = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(contents.contains('─'));
        for corner in ['╭', '╮', '╰', '╯'] {
            assert!(!contents.contains(corner));
        }
    }
}
