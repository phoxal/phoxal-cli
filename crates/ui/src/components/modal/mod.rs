mod model;

pub use model::ModalModel;

use tuirealm::ratatui::Frame;
use tuirealm::ratatui::layout::{Alignment, Constraint, Flex, Layout, Rect};
use tuirealm::ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};

use crate::Theme;
use crate::app::{AppModel, ModalId};

pub fn render(frame: &mut Frame, area: Rect, model: &AppModel, theme: Theme) {
    let Some(modal) = &model.modal else {
        return;
    };
    let [horizontal] = Layout::horizontal([Constraint::Percentage(70)])
        .flex(Flex::Center)
        .areas(area);
    let [popup] = Layout::vertical([Constraint::Percentage(60)])
        .flex(Flex::Center)
        .areas(horizontal);
    let (title, body): (&str, String) = match modal.id {
        ModalId::Help => (
            " Help ",
            "Enter descends from tabs to panels to content. Esc restores the previous depth. Panel-local shortcuts work only after entering that panel.\n\nRuntimes: arrows, Enter detail, r restart, l logs\nLogs: filters / f s, stream arrows/End/Space\nBus: / s a and arrows\nInput: arrows, Enter select, e enable, x disable, r rescan\nS: explicit stop confirmation; q: detach only".to_string(),
        ),
        ModalId::SessionInfo => {
            let info = model.overview.supervisor.as_ref().map_or_else(
                || "Attachment snapshot not received".to_string(),
                |snapshot| {
                    format!(
                        "project: {}\nentry: {}\nframework: {}\nrouter: {}\nexecution: {}\ngraph generation: {}",
                        snapshot.project,
                        snapshot.entry,
                        snapshot.framework_train,
                        snapshot.router,
                        snapshot.execution_id,
                        snapshot.graph_generation
                    )
                },
            );
            (" Session info ", info)
        }
        ModalId::ConfirmStop => (
            " Stop project? ",
            "Enter stops the resident and every supervised process. Esc cancels. Closing the UI with q never stops the resident.".to_string(),
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
                    .border_style(crate::theme::role::fg(theme, crate::Role::Accent)),
            ),
        popup,
    );
}
