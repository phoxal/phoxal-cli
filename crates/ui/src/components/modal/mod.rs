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
            "Enter descends from tabs to panels to content. Esc restores the previous depth. Panel-local shortcuts work only after entering that panel.\n\nRuntimes: arrows, Enter detail, r restart, l logs\nLogs: filters / f s, stream arrows/End/Space\nBus: / s a and arrows\nInput: arrows, Enter select, e enable, x disable, r rescan\nCtrl+C: stop confirmation; q: detach only".to_string(),
        ),
        ModalId::SessionInfo => {
            let info = model.overview.supervisor.as_ref().map_or_else(
                || "Attachment snapshot not received".to_string(),
                |snapshot| {
                    format!(
                        "project: {}\nrobot: {}/{}\nclock: {:?}\nexecution: {}\nrevision: {}",
                        sanitize(&snapshot.project),
                        sanitize(snapshot.robot.namespace.as_str()),
                        sanitize(snapshot.robot.id.as_str()),
                        snapshot.mode,
                        snapshot.execution,
                        snapshot.revision
                    )
                },
            );
            (" Session info ", info)
        }
        ModalId::ConfirmStop => (
            " Stop project? ",
            "Enter stops the execution and every supervised process. Esc cancels. Closing the UI with q only detaches - the daemon keeps running.".to_string(),
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
}
