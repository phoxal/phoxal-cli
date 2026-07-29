//! Closed message vocabulary for the attachment application.

use phoxal_cli_observation::{AttachmentEvent, BusWindow, LogWindow, RuntimeWindow};
use tuirealm::event::KeyEvent;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NavigationMsg {
    Key(KeyEvent),
    Refresh { clear: bool },
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
    Runtimes(RuntimesMsg),
    Logs(LogsMsg),
    Bus(BusMsg),
    Diagnostic(String),
    Wake,
    Terminate,
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
