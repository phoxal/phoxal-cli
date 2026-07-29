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
                snapshot.project.clone(),
                format!("{:?}", snapshot.lifecycle).to_lowercase(),
            )
        },
    );
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(" phoxal ", crate::ratatui::selected(theme, Role::Accent)),
            Span::raw(project),
            Span::raw("  "),
            Span::styled(lifecycle, crate::ratatui::fg(theme, Role::Steel)),
        ]))
        .block(Block::default().borders(Borders::BOTTOM)),
        area,
    );
}

pub fn render_tabs(frame: &mut Frame, area: Rect, model: &AppModel, theme: Theme) {
    let page = model.route.page();
    let tab_focus = matches!(model.route, FocusRoute::Tabs { .. });
    let tabs = PageId::ALL
        .iter()
        .enumerate()
        .flat_map(|(index, candidate)| {
            let selected = *candidate == page;
            let marker = if selected && tab_focus {
                ">"
            } else if selected {
                "*"
            } else {
                " "
            };
            [
                Span::styled(
                    format!("{marker}{} {}", index + 1, candidate.label()),
                    if selected {
                        crate::ratatui::selected(theme, Role::Accent)
                    } else {
                        crate::ratatui::muted(theme)
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
    frame.render_widget(
        Paragraph::new(format!(
            " {depth}  Enter descend  Esc ascend  ? help  i session  S stop  q detach"
        ))
        .style(crate::ratatui::muted(theme))
        .block(Block::default().borders(Borders::TOP)),
        area,
    );
}
