use std::cell::RefCell;
use std::marker::PhantomData;
use std::rc::Rc;

use tuirealm::command::{Cmd, CmdResult};
use tuirealm::component::{AppComponent, Component};
use tuirealm::event::Event;
use tuirealm::props::{AttrValue, Attribute, QueryResult};
use tuirealm::ratatui::Frame;
use tuirealm::ratatui::layout::Rect;
use tuirealm::state::State;

use crate::Theme;
use crate::app::AppModel;
use crate::app::Msg;
use crate::app::subscriptions::UserEvent;

pub trait RenderRegion {
    fn render(frame: &mut Frame, area: Rect, model: &AppModel, theme: Theme);
}

/// A component holds a typed model handle; tui-realm's stringly property bag is
/// deliberately unused for Phoxal state.
pub struct RenderComponent<R> {
    model: Rc<RefCell<AppModel>>,
    theme: Theme,
    region: PhantomData<R>,
}

impl<R> RenderComponent<R> {
    #[must_use]
    pub fn new(model: Rc<RefCell<AppModel>>, theme: Theme) -> Self {
        Self {
            model,
            theme,
            region: PhantomData,
        }
    }
}

impl<R: RenderRegion> Component for RenderComponent<R> {
    fn view(&mut self, frame: &mut Frame, area: Rect) {
        let model = self.model.borrow();
        R::render(frame, area, &model, self.theme);
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

impl<R: RenderRegion + 'static> AppComponent<Msg, UserEvent> for RenderComponent<R> {
    fn on(&mut self, _event: &Event<UserEvent>) -> Option<Msg> {
        None
    }
}

macro_rules! render_region {
    ($name:ident, $render:path) => {
        pub struct $name;

        impl RenderRegion for $name {
            fn render(frame: &mut Frame, area: Rect, model: &AppModel, theme: Theme) {
                $render(frame, area, model, theme);
            }
        }
    };
}

render_region!(HeaderRegion, crate::components::chrome::render_header);
render_region!(TabsRegion, crate::components::chrome::render_tabs);
render_region!(OverviewRegion, crate::components::overview::render);
render_region!(RuntimesRegion, crate::components::runtimes::render);
render_region!(LogsRegion, crate::components::logs::render);
render_region!(BusRegion, crate::components::bus::render);
render_region!(InputRegion, crate::components::input::render);
render_region!(ModalRegion, crate::components::modal::render);
render_region!(FooterRegion, crate::components::chrome::render_footer);

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
    let marker = panel_marker(model, panel);
    let focused = marker != " ";
    outer_panel_block(format!("{marker} {title}"), theme).border_style(if focused {
        crate::theme::role::fg(theme, crate::Role::Accent)
    } else {
        crate::theme::role::muted(theme)
    })
}

#[must_use]
pub fn outer_panel_block<'a>(
    title: impl Into<tuirealm::ratatui::text::Line<'a>>,
    theme: Theme,
) -> tuirealm::ratatui::widgets::Block<'a> {
    use tuirealm::ratatui::widgets::{Block, BorderType, Borders};

    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .title(title)
        .border_style(crate::theme::role::muted(theme))
}

#[cfg(test)]
mod border_tests {
    use super::*;
    use tuirealm::ratatui::Terminal;
    use tuirealm::ratatui::backend::TestBackend;

    #[test]
    fn centralized_outer_panel_uses_all_rounded_corners() {
        let mut terminal = Terminal::new(TestBackend::new(20, 5)).unwrap();
        terminal
            .draw(|frame| {
                frame.render_widget(
                    outer_panel_block(" Panel ", Theme::new(crate::ColorCapability::None)),
                    frame.area(),
                );
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(0, 0)].symbol(), "╭");
        assert_eq!(buffer[(19, 0)].symbol(), "╮");
        assert_eq!(buffer[(0, 4)].symbol(), "╰");
        assert_eq!(buffer[(19, 4)].symbol(), "╯");
    }
}
