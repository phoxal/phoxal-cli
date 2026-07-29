//! Closed message vocabulary for the attachment application.

use phoxal_cli_observation::{AttachmentEvent, BusWindow, LogWindow, RuntimeWindow};
use tuirealm::event::KeyEvent;

use super::id::{ModalId, PageId};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NavigationMsg {
    Key(KeyEvent),
    Resize { width: u16, height: u16 },
    Page(PageId),
    OpenModal(ModalId),
    CloseModal,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OverviewMsg {
    Acknowledge,
}

#[derive(Debug, Clone, PartialEq)]
pub enum RuntimesMsg {
    Window(RuntimeWindow),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogsMsg {
    Window(LogWindow),
}

#[derive(Debug, Clone, PartialEq)]
pub enum BusMsg {
    Window(BusWindow),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputMsg {
    Acknowledge,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModalMsg {
    Confirm,
    Cancel,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SessionInput {
    Client(AttachmentEvent),
    Logs(LogWindow),
    Bus(BusWindow),
    Runtimes(RuntimeWindow),
    Diagnostic(String),
    Terminate,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Msg {
    Client(AttachmentEvent),
    Navigate(NavigationMsg),
    Overview(OverviewMsg),
    Runtimes(RuntimesMsg),
    Logs(LogsMsg),
    Bus(BusMsg),
    Input(InputMsg),
    Modal(ModalMsg),
    Diagnostic(String),
    Wake,
    Terminate,
    Quit,
}

impl From<SessionInput> for Msg {
    fn from(value: SessionInput) -> Self {
        match value {
            SessionInput::Client(event) => Self::Client(event),
            SessionInput::Logs(window) => Self::Logs(LogsMsg::Window(window)),
            SessionInput::Bus(window) => Self::Bus(BusMsg::Window(window)),
            SessionInput::Runtimes(window) => Self::Runtimes(RuntimesMsg::Window(window)),
            SessionInput::Diagnostic(message) => Self::Diagnostic(message),
            SessionInput::Terminate => Self::Terminate,
        }
    }
}
