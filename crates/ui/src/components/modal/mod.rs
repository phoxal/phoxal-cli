use tuirealm::ratatui::Frame;
use tuirealm::ratatui::layout::{Alignment, Constraint, Flex, Layout, Rect};
use tuirealm::ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap};

use crate::Theme;
use crate::app::{AppModel, FocusRoute, ModalId};

pub fn render(frame: &mut Frame, area: Rect, model: &AppModel, theme: Theme) {
    let FocusRoute::Modal { modal, .. } = &model.route else {
        return;
    };
    let [horizontal] = Layout::horizontal([Constraint::Percentage(70)])
        .flex(Flex::Center)
        .areas(area);
    let [popup] = Layout::vertical([Constraint::Percentage(60)])
        .flex(Flex::Center)
        .areas(horizontal);
    let (title, body): (&str, String) = match modal {
        ModalId::Help => (
            " Help ",
            format!(
                "Enter descends from tabs to panels to content. Esc restores the previous depth. Panel-local shortcuts work only after entering that panel.\n\nRuntimes: arrows, Enter detail, r restart, l logs\nLogs: filters / f s, stream arrows/End/Space\nBus: / s a and arrows\nInput: arrows, Enter select, e enable, x disable, r rescan\nCtrl+C: stop confirmation; {}",
                quit_meaning(model.detachable)
            ),
        ),
        ModalId::SessionInfo => {
            let info = model.overview.supervisor.as_ref().map_or_else(
                || "Attachment snapshot not received".to_string(),
                |snapshot| {
                    format!(
                        "project: {}\nrobot: {}\nclock: {:?}\nexecution: {}\nrevision: {}",
                        sanitize(&snapshot.project),
                        sanitize(snapshot.robot.as_str()),
                        snapshot.clock,
                        snapshot.execution,
                        snapshot.revision
                    )
                },
            );
            (" Session info ", info)
        }
        ModalId::ConfirmStop => (
            " Stop project? ",
            format!(
                "Enter stops the execution and every supervised process. Esc cancels. {}",
                quit_meaning(model.detachable)
            ),
        ),
    };
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(body)
            .alignment(Alignment::Left)
            .wrap(Wrap { trim: true })
            .block(
                Block::default()
                    .title(title)
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(crate::theme::role::fg(theme, crate::Role::Accent)),
            ),
        popup,
    );
}

fn sanitize(value: &str) -> String {
    crate::format::sanitize_terminal_text(value)
}

/// The sentence that spells out what leaving costs in this session.
const fn quit_meaning(detachable: bool) -> &'static str {
    if detachable {
        "closing the UI with q only detaches - the supervisor keeps running."
    } else {
        "q ends the simulation session: the execution is stopped and Webots is closed."
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ColorCapability;
    use crate::app::{FocusRoute, ModalId, PageId};
    use tuirealm::ratatui::Terminal;
    use tuirealm::ratatui::backend::TestBackend;

    #[test]
    fn modal_outer_box_has_rounded_corners() {
        let mut terminal = Terminal::new(TestBackend::new(40, 12)).unwrap();
        let model = AppModel {
            route: FocusRoute::Tabs {
                page: PageId::Overview,
                candidate: PageId::Overview,
            }
            .open_modal(ModalId::Help),
            ..AppModel::default()
        };
        terminal
            .draw(|frame| {
                render(
                    frame,
                    frame.area(),
                    &model,
                    Theme::new(ColorCapability::None),
                );
            })
            .unwrap();
        let contents = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        for corner in ["╭", "╮", "╰", "╯"] {
            assert!(contents.contains(corner), "missing {corner}");
        }
    }

    /// Both modals explain leaving in this session's own terms; neither may
    /// promise a detach a simulation session cannot perform.
    #[test]
    fn the_modals_explain_what_leaving_costs_in_this_session() {
        assert!(quit_meaning(true).contains("only detaches"));
        assert!(quit_meaning(false).contains("ends the simulation session"));

        for modal in [ModalId::Help, ModalId::ConfirmStop] {
            let model = AppModel {
                detachable: false,
                route: FocusRoute::Tabs {
                    page: PageId::Overview,
                    candidate: PageId::Overview,
                }
                .open_modal(modal),
                ..AppModel::default()
            };
            // Wide enough that the sentence under test is not word-wrapped
            // into pieces the assertion would have to reassemble.
            let mut terminal = Terminal::new(TestBackend::new(300, 30)).unwrap();
            terminal
                .draw(|frame| {
                    render(
                        frame,
                        frame.area(),
                        &model,
                        Theme::new(ColorCapability::None),
                    );
                })
                .unwrap();
            let contents = terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>();
            assert!(
                contents.contains("ends the simulation session"),
                "{contents}"
            );
            assert!(!contents.contains("only detaches"), "{contents}");
        }
    }
}
