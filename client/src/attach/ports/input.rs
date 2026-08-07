use anyhow::{Context, Result};
use tokio::sync::mpsc;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum InputCommand {
    Select(String),
    SetEnabled(bool),
    Rescan,
}

#[derive(Clone)]
pub(crate) struct InputCommands {
    sender: mpsc::Sender<InputCommand>,
}

impl InputCommands {
    pub(crate) const fn new(sender: mpsc::Sender<InputCommand>) -> Self {
        Self { sender }
    }

    pub async fn select(&self, id: impl Into<String>) -> Result<()> {
        self.send(InputCommand::Select(id.into())).await
    }

    pub async fn set_enabled(&self, enabled: bool) -> Result<()> {
        self.send(InputCommand::SetEnabled(enabled)).await
    }

    pub async fn rescan(&self) -> Result<()> {
        self.send(InputCommand::Rescan).await
    }

    async fn send(&self, command: InputCommand) -> Result<()> {
        self.sender
            .send(command)
            .await
            .context("attachment input command port is closed")
    }
}
