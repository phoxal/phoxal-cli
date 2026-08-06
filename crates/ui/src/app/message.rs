//! Closed message vocabulary for the attachment application.

use phoxal_cli_observation::{AttachmentEvent, LogWindow, RuntimeWindow};
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
pub enum SessionInput {
    Client(AttachmentEvent),
    Logs(LogWindow),
    Runtimes(RuntimeWindow),
    Diagnostic(String),
    /// The operator pressed Ctrl+C. The UI decides what it means; the host
    /// only reports that it happened (organization#978).
    Interrupt,
    Terminate,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Msg {
    Client(AttachmentEvent),
    Navigate(NavigationMsg),
    Runtimes(RuntimesMsg),
    Logs(LogsMsg),
    Diagnostic(String),
    Wake,
    Interrupt,
    Terminate,
}

impl From<SessionInput> for Msg {
    fn from(value: SessionInput) -> Self {
        match value {
            SessionInput::Client(event) => Self::Client(event),
            SessionInput::Logs(window) => Self::Logs(LogsMsg::Window(window)),
            SessionInput::Runtimes(window) => Self::Runtimes(RuntimesMsg::Window(window)),
            SessionInput::Diagnostic(message) => Self::Diagnostic(message),
            SessionInput::Interrupt => Self::Interrupt,
            SessionInput::Terminate => Self::Terminate,
        }
    }
}
