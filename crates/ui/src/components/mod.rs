//! Typed tui-realm components. Components render and emit messages; they do no I/O.

pub mod chrome;
pub mod input;
pub mod logs;
pub mod modal;
pub mod overview;
pub mod runtimes;
pub mod shared;

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::SystemTime;

    use phoxal_cli_observation::{
        InputObservation, JoypadDevice, JoypadDevicesSample, LogRow, LogSeverity, LogSource,
        RuntimePerformanceSample, RuntimeRow,
    };
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
                    PageId::Input => {
                        super::input::render(frame, Rect::new(0, 0, 100, 32), &model, theme)
                    }
                })
                .expect("render page");
        }
    }

    #[test]
    fn every_page_component_renders_non_empty_typed_state() {
        let mut terminal = Terminal::new(TestBackend::new(100, 32)).expect("test terminal");
        let mut model = AppModel::default();
        model
            .overview
            .push_diagnostic("visible-diagnostic".to_string());
        model.logs.rows.push(LogRow {
            participant: "drive".to_string(),
            source: LogSource::Raw,
            severity: LogSeverity::Info,
            text: "log-token".to_string(),
            event_time: SystemTime::UNIX_EPOCH,
        });
        model.runtimes.rows.push(RuntimeRow {
            sample: RuntimePerformanceSample {
                record: phoxal_client::supervisor::telemetry::Record {
                    sequence: 1,
                    participant_id: "runtime-token".to_string(),
                    truncated: 0,
                    window_ns: 1,
                    step: None,
                    topics: Vec::new(),
                    overflow: None,
                },
            },
            capacity_evictions: 0,
        });
        model.input.observation = Some(InputObservation {
            joypads: JoypadDevicesSample {
                available: Arc::new(vec![JoypadDevice {
                    id: "pad-token".to_string(),
                    name: "PadToken".to_string(),
                    ..JoypadDevice::default()
                }]),
                ..JoypadDevicesSample::default()
            },
            motion: None,
        });
        let theme = Theme::new(ColorCapability::None);
        for (page, expected) in [
            (PageId::Overview, "visible-diagnostic"),
            (PageId::Runtimes, "runtime-token"),
            (PageId::Logs, "log-token"),
            (PageId::Input, "PadToken"),
        ] {
            terminal.clear().expect("clear test terminal");
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
                    PageId::Input => {
                        super::input::render(frame, Rect::new(0, 0, 100, 32), &model, theme)
                    }
                })
                .expect("render page");
            let contents = terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>();
            assert!(contents.contains(expected), "{page:?} missed {expected}");
        }
    }
}
