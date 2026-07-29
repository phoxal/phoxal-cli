//! Typed tui-realm components. Components render and emit messages; they do no I/O.

pub mod bus;
pub mod chrome;
pub mod input;
pub mod logs;
pub mod modal;
pub mod overview;
pub mod runtimes;
pub mod shared;

#[cfg(test)]
mod tests {
    use tuirealm::ratatui::Terminal;
    use tuirealm::ratatui::backend::TestBackend;
    use tuirealm::ratatui::layout::Rect;

    use crate::app::{AppModel, PageId};
    use crate::{ColorCapability, Theme};

    #[test]
    fn every_page_component_renders_empty_typed_state() {
        let mut terminal = Terminal::new(TestBackend::new(100, 32)).expect("test terminal");
        let model = AppModel::default();
        let theme = Theme::new(ColorCapability::None);
        for page in PageId::ALL {
            terminal
                .draw(|frame| match page {
                    PageId::Overview => {
                        super::overview::render(frame, Rect::new(0, 0, 100, 32), &model, theme)
                    }
                    PageId::Runtimes => {
                        super::runtimes::render(frame, Rect::new(0, 0, 100, 32), &model, theme)
                    }
                    PageId::Logs => {
                        super::logs::render(frame, Rect::new(0, 0, 100, 32), &model, theme)
                    }
                    PageId::Bus => {
                        super::bus::render(frame, Rect::new(0, 0, 100, 32), &model, theme)
                    }
                    PageId::Input => {
                        super::input::render(frame, Rect::new(0, 0, 100, 32), &model, theme)
                    }
                })
                .expect("render page");
        }
    }
}
