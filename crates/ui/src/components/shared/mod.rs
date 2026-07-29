use std::sync::{Arc, Mutex};

use tuirealm::command::{Cmd, CmdResult};
use tuirealm::component::{AppComponent, Component};
use tuirealm::event::Event;
use tuirealm::props::{AttrValue, Attribute, QueryResult};
use tuirealm::ratatui::Frame;
use tuirealm::ratatui::layout::Rect;
use tuirealm::state::State;

use crate::Theme;
use crate::app::Msg;
use crate::app::subscriptions::UserEvent;
use crate::app::{AppModel, PageId};

#[derive(Debug, Clone, Copy)]
pub enum Region {
    Header,
    Tabs,
    Page(PageId),
    Modal,
    Footer,
}

/// A component holds a typed model handle; tui-realm's stringly property bag is
/// deliberately unused for Phoxal state.
pub struct RenderComponent {
    model: Arc<Mutex<AppModel>>,
    theme: Theme,
    region: Region,
}

impl RenderComponent {
    #[must_use]
    pub fn new(model: Arc<Mutex<AppModel>>, theme: Theme, region: Region) -> Self {
        Self {
            model,
            theme,
            region,
        }
    }
}

impl Component for RenderComponent {
    fn view(&mut self, frame: &mut Frame, area: Rect) {
        let Ok(model) = self.model.lock() else {
            return;
        };
        match self.region {
            Region::Header => {
                crate::components::chrome::render_header(frame, area, &model, self.theme)
            }
            Region::Tabs => crate::components::chrome::render_tabs(frame, area, &model, self.theme),
            Region::Page(PageId::Overview) => {
                crate::components::overview::render(frame, area, &model, self.theme);
            }
            Region::Page(PageId::Runtimes) => {
                crate::components::runtimes::render(frame, area, &model, self.theme);
            }
            Region::Page(PageId::Logs) => {
                crate::components::logs::render(frame, area, &model, self.theme);
            }
            Region::Page(PageId::Bus) => {
                crate::components::bus::render(frame, area, &model, self.theme);
            }
            Region::Page(PageId::Input) => {
                crate::components::input::render(frame, area, &model, self.theme);
            }
            Region::Modal => crate::components::modal::render(frame, area, &model, self.theme),
            Region::Footer => {
                crate::components::chrome::render_footer(frame, area, &model, self.theme)
            }
        }
    }

    fn query(&self, _attr: Attribute) -> Option<QueryResult<'_>> {
        None
    }

    fn attr(&mut self, _attr: Attribute, _value: AttrValue) {}

    fn state(&self) -> State {
        State::None
    }

    fn perform(&mut self, cmd: Cmd) -> CmdResult {
        CmdResult::Invalid(cmd)
    }
}

impl AppComponent<Msg, UserEvent> for RenderComponent {
    fn on(&mut self, _event: &Event<UserEvent>) -> Option<Msg> {
        None
    }
}

#[must_use]
pub fn panel_marker(model: &AppModel, panel: crate::app::PanelId) -> &'static str {
    match &model.route {
        crate::app::FocusRoute::Panels {
            candidate: Some(candidate),
            ..
        } if *candidate == panel => ">",
        crate::app::FocusRoute::Content { panel: entered } if *entered == panel => "*",
        _ => " ",
    }
}

#[must_use]
pub fn panel_block<'a>(
    model: &AppModel,
    panel: crate::app::PanelId,
    title: &'a str,
    theme: Theme,
) -> tuirealm::ratatui::widgets::Block<'a> {
    use tuirealm::ratatui::widgets::{Block, Borders};

    let marker = panel_marker(model, panel);
    let focused = marker != " ";
    Block::default()
        .borders(Borders::ALL)
        .title(format!("{marker} {title}"))
        .border_style(if focused {
            crate::theme::role::fg(theme, crate::Role::Accent)
        } else {
            crate::theme::role::muted(theme)
        })
}
