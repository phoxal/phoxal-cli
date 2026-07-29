use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use phoxal_cli_protocol::codec::async_io::{read_frame, write_frame};
use phoxal_cli_protocol::limits::{
    FRAME_READ_TIMEOUT, FRAME_WRITE_TIMEOUT, MAX_COMMAND_FRAME_BYTES,
};
use phoxal_cli_protocol::{
    CommandAction, CommandKey, CommandReply, CommandRequest, CommandSessionId, ConnectionRole,
};
use tokio::net::UnixStream;
use tokio::sync::Mutex;

use super::connection::connect_role;

struct CommandChannel {
    stream: Option<UnixStream>,
    generation: u64,
    session: CommandSessionId,
    next_sequence: u64,
}

#[derive(Clone)]
pub struct SupervisorCommands {
    path: PathBuf,
    command: Arc<Mutex<CommandChannel>>,
}

impl SupervisorCommands {
    pub async fn connect(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        let (stream, handshake) = connect_role(&path, ConnectionRole::Commands, None).await?;
        let session = handshake
            .command_session
            .context("command handshake did not issue a session")?;
        Ok(Self {
            path,
            command: Arc::new(Mutex::new(CommandChannel {
                stream: Some(stream),
                generation: handshake.supervisor_generation,
                session,
                next_sequence: 1,
            })),
        })
    }

    pub async fn command(&self, action: CommandAction) -> Result<CommandReply> {
        let mut channel = self.command.lock().await;
        let sequence = channel.next_sequence;
        let request = CommandRequest {
            supervisor_generation: channel.generation,
            key: CommandKey {
                session: channel.session,
                sequence,
            },
            action,
        };
        let stream = channel.stream.as_mut().context(
            "supervisor command channel is unavailable; reconnect before issuing another action",
        )?;
        if let Err(error) = write_frame(
            stream,
            &request,
            MAX_COMMAND_FRAME_BYTES,
            FRAME_WRITE_TIMEOUT,
        )
        .await
        {
            channel.stream = None;
            return Err(error).context(
                "write supervisor command; command outcome is unknown, reconnect before issuing another action",
            );
        }
        match read_frame(
            channel.stream.as_mut().expect("stream survived write"),
            MAX_COMMAND_FRAME_BYTES,
            FRAME_READ_TIMEOUT,
        )
        .await
        {
            Ok(reply) => {
                channel.next_sequence = sequence.saturating_add(1);
                Ok(reply)
            }
            Err(error) => {
                channel.stream = None;
                Err(error).context(
                    "read supervisor command reply; command outcome is unknown, reconnect before issuing another action",
                )
            }
        }
    }

    pub async fn command_with_reconnect(&self, action: CommandAction) -> Result<CommandReply> {
        match self.command(action.clone()).await {
            Ok(reply) => Ok(reply),
            Err(first_error) => {
                self.reconnect().await?;
                self.command(action).await.with_context(|| {
                    format!("retry supervisor command after transport failure ({first_error:#})")
                })
            }
        }
    }

    /// Resume the existing command session and preserve its sequence watermark.
    ///
    /// Retrying the same `(session, sequence)` is safe when a transport failure
    /// leaves the first command outcome unknown: the resident returns its
    /// cached reply instead of executing the action twice.
    pub async fn reconnect(&self) -> Result<()> {
        let mut channel = self.command.lock().await;
        let session = channel.session;
        let next_sequence = channel.next_sequence;
        let (stream, handshake) =
            connect_role(&self.path, ConnectionRole::Commands, Some(session)).await?;
        let resumed_session = handshake
            .command_session
            .context("command handshake did not issue a session")?;
        anyhow::ensure!(
            resumed_session == session,
            "resident did not resume the requested command session"
        );
        *channel = CommandChannel {
            stream: Some(stream),
            generation: handshake.supervisor_generation,
            session: resumed_session,
            next_sequence,
        };
        Ok(())
    }
}
