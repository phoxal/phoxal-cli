//! Project-local client for the resident Phoxal supervisor.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use phoxal_cli_core::session::protocol::{
    MAX_COMMAND_FRAME_BYTES, MAX_HANDSHAKE_FRAME_BYTES, MAX_SNAPSHOT_FRAME_BYTES,
    SUPERVISOR_PROTOCOL_VERSION,
};
use phoxal_cli_core::session::{
    CommandAction, CommandKey, CommandReply, CommandRequest, CommandSessionId, ConnectionRole,
    HandshakeReply, HandshakeRequest, SupervisorSnapshotV0,
};
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::sync::{Mutex, watch};

pub const FRAME_READ_TIMEOUT: Duration = Duration::from_secs(5);
pub const FRAME_WRITE_TIMEOUT: Duration = Duration::from_secs(5);
const RECONNECT_DELAY: Duration = Duration::from_millis(200);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionState {
    Connected,
    Reconnecting,
    Terminal,
    Crashed,
}

#[derive(Clone)]
pub struct SnapshotStore {
    snapshots: watch::Receiver<SupervisorSnapshotV0>,
    connection: watch::Receiver<ConnectionState>,
}

impl SnapshotStore {
    #[must_use]
    pub fn current(&self) -> SupervisorSnapshotV0 {
        self.snapshots.borrow().clone()
    }

    #[must_use]
    pub const fn snapshots(&self) -> &watch::Receiver<SupervisorSnapshotV0> {
        &self.snapshots
    }

    pub fn subscribe(&self) -> watch::Receiver<SupervisorSnapshotV0> {
        self.snapshots.clone()
    }

    pub fn connection(&self) -> watch::Receiver<ConnectionState> {
        self.connection.clone()
    }
}

struct CommandChannel {
    stream: Option<UnixStream>,
    generation: u64,
    session: CommandSessionId,
    next_sequence: u64,
}

#[derive(Clone)]
pub struct SupervisorClient {
    path: PathBuf,
    snapshots: SnapshotStore,
    command: Arc<Mutex<CommandChannel>>,
}

impl SupervisorClient {
    pub async fn connect(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        let (mut snapshot_stream, snapshot_handshake) =
            connect_role(&path, ConnectionRole::Snapshots, None).await?;
        let initial = read_frame::<_, SupervisorSnapshotV0>(
            &mut snapshot_stream,
            MAX_SNAPSHOT_FRAME_BYTES,
            FRAME_READ_TIMEOUT,
        )
        .await
        .context("read initial supervisor snapshot")?;
        anyhow::ensure!(
            initial.supervisor_generation == snapshot_handshake.supervisor_generation,
            "snapshot generation does not match handshake"
        );
        let (snapshot_tx, snapshot_rx) = watch::channel(initial);
        let (connection_tx, connection_rx) = watch::channel(ConnectionState::Connected);
        tokio::spawn(snapshot_loop(
            path.clone(),
            snapshot_stream,
            snapshot_tx,
            connection_tx,
        ));

        let (command_stream, command_handshake) =
            connect_role(&path, ConnectionRole::Commands, None).await?;
        let session = command_handshake
            .command_session
            .context("command handshake did not issue a session")?;
        Ok(Self {
            path,
            snapshots: SnapshotStore {
                snapshots: snapshot_rx,
                connection: connection_rx,
            },
            command: Arc::new(Mutex::new(CommandChannel {
                stream: Some(command_stream),
                generation: command_handshake.supervisor_generation,
                session,
                next_sequence: 1,
            })),
        })
    }

    #[must_use]
    pub fn snapshots(&self) -> SnapshotStore {
        self.snapshots.clone()
    }

    pub async fn command(&self, action: CommandAction) -> Result<CommandReply> {
        let mut channel = self.command.lock().await;
        let generation = channel.generation;
        let session = channel.session;
        let sequence = channel.next_sequence;
        let request = CommandRequest {
            supervisor_generation: generation,
            key: CommandKey { session, sequence },
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
        // Once the complete request has been written, this sequence can
        // never name another action even if the reply is lost.
        channel.next_sequence = sequence.saturating_add(1);
        let reply = match channel.stream.as_mut() {
            Some(stream) => read_frame(stream, MAX_COMMAND_FRAME_BYTES, FRAME_READ_TIMEOUT).await,
            None => unreachable!("command stream was present for the successful write"),
        };
        match reply {
            Ok(reply) => Ok(reply),
            Err(error) => {
                channel.stream = None;
                Err(error).context(
                    "read supervisor command reply; command outcome is unknown, reconnect before issuing another action",
                )
            }
        }
    }

    /// Issue a command and recover once from a command-role transport fault.
    ///
    /// The retry always uses a newly issued command session. It is therefore
    /// suitable only for idempotent actions, or actions such as restart whose
    /// expected-producer precondition makes replay safe. The ambiguous
    /// sequence from the failed session is never reused.
    pub async fn command_with_reconnect(&self, action: CommandAction) -> Result<CommandReply> {
        match self.command(action.clone()).await {
            Ok(reply) => Ok(reply),
            Err(first_error) => {
                self.reconnect_commands()
                    .await
                    .context("reconnect command channel after transport failure")?;
                self.command(action).await.with_context(|| {
                    format!("retry supervisor command after transport failure ({first_error:#})")
                })
            }
        }
    }

    /// Reconnect the command role after a transport failure.
    ///
    /// This never retries an action: the caller must first reconstruct truth
    /// from the snapshot store, then explicitly issue a new command.
    pub async fn reconnect_commands(&self) -> Result<()> {
        let (stream, handshake) = connect_role(&self.path, ConnectionRole::Commands, None).await?;
        let session = handshake
            .command_session
            .context("command handshake did not issue a session")?;
        *self.command.lock().await = CommandChannel {
            stream: Some(stream),
            generation: handshake.supervisor_generation,
            session,
            next_sequence: 1,
        };
        Ok(())
    }
}

async fn snapshot_loop(
    path: PathBuf,
    mut stream: UnixStream,
    snapshots: watch::Sender<SupervisorSnapshotV0>,
    connection: watch::Sender<ConnectionState>,
) {
    loop {
        match read_snapshot_frame(&mut stream).await {
            Ok(snapshot) => {
                snapshots.send_replace(snapshot);
                connection.send_replace(ConnectionState::Connected);
            }
            Err(_) => {
                let terminal = matches!(
                    snapshots.borrow().lifecycle,
                    phoxal_cli_core::session::ProjectLifecycle::Stopped
                        | phoxal_cli_core::session::ProjectLifecycle::Failed
                );
                if terminal {
                    connection.send_replace(ConnectionState::Terminal);
                    return;
                }
                connection.send_replace(ConnectionState::Reconnecting);
                tokio::time::sleep(RECONNECT_DELAY).await;
                match connect_role(&path, ConnectionRole::Snapshots, None).await {
                    Ok((new_stream, handshake)) => {
                        stream = new_stream;
                        match read_snapshot_frame(&mut stream).await {
                            Ok(snapshot)
                                if snapshot.supervisor_generation
                                    == handshake.supervisor_generation =>
                            {
                                snapshots.send_replace(snapshot);
                                connection.send_replace(ConnectionState::Connected);
                            }
                            _ => {
                                connection.send_replace(ConnectionState::Crashed);
                                return;
                            }
                        }
                    }
                    Err(_) => {
                        connection.send_replace(ConnectionState::Crashed);
                        return;
                    }
                }
            }
        }
    }
}

/// Snapshot streams may be legitimately idle forever. The deadline begins
/// only once a frame header arrives; a partial/malicious frame remains
/// bounded without inventing protocol heartbeat traffic.
async fn read_snapshot_frame(stream: &mut UnixStream) -> Result<SupervisorSnapshotV0> {
    read_frame_after_idle(stream, MAX_SNAPSHOT_FRAME_BYTES, FRAME_READ_TIMEOUT).await
}

pub async fn connect_role(
    path: &Path,
    role: ConnectionRole,
    resume_command_session: Option<CommandSessionId>,
) -> Result<(UnixStream, HandshakeReply)> {
    let mut stream = UnixStream::connect(path)
        .await
        .with_context(|| format!("connect to {}", path.display()))?;
    write_frame(
        &mut stream,
        &HandshakeRequest {
            protocol_version: SUPERVISOR_PROTOCOL_VERSION,
            role,
            resume_command_session,
        },
        MAX_HANDSHAKE_FRAME_BYTES,
        FRAME_WRITE_TIMEOUT,
    )
    .await?;
    let reply: HandshakeReply =
        read_frame(&mut stream, MAX_HANDSHAKE_FRAME_BYTES, FRAME_READ_TIMEOUT).await?;
    if reply.protocol_version != SUPERVISOR_PROTOCOL_VERSION {
        bail!(
            "supervisor protocol version {} is not supported by this client (expected {})",
            reply.protocol_version,
            SUPERVISOR_PROTOCOL_VERSION
        );
    }
    Ok((stream, reply))
}

pub async fn write_frame<W, T>(
    writer: &mut W,
    value: &T,
    maximum: usize,
    deadline: Duration,
) -> Result<()>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    let bytes = serde_json::to_vec(value).context("encode protocol frame")?;
    anyhow::ensure!(
        bytes.len() <= maximum,
        "encoded protocol frame is {} bytes; limit is {maximum}",
        bytes.len()
    );
    let length = u32::try_from(bytes.len()).context("protocol frame length exceeds u32")?;
    tokio::time::timeout(deadline, async {
        writer.write_all(&length.to_be_bytes()).await?;
        writer.write_all(&bytes).await?;
        writer.flush().await
    })
    .await
    .context("protocol frame write timed out")??;
    Ok(())
}

pub async fn read_frame<R, T>(reader: &mut R, maximum: usize, deadline: Duration) -> Result<T>
where
    R: AsyncRead + Unpin,
    T: DeserializeOwned,
{
    let mut length = [0_u8; 4];
    tokio::time::timeout(deadline, reader.read_exact(&mut length))
        .await
        .context("protocol frame length read timed out")??;
    let length = u32::from_be_bytes(length) as usize;
    anyhow::ensure!(
        length <= maximum,
        "protocol frame declares {length} bytes; limit is {maximum}"
    );
    let mut bytes = vec![0_u8; length];
    tokio::time::timeout(deadline, reader.read_exact(&mut bytes))
        .await
        .context("protocol frame body read timed out")??;
    serde_json::from_slice(&bytes).context("decode protocol frame")
}

/// Read a frame from a connection that may be legitimately idle indefinitely.
///
/// Waiting for the first header byte is unbounded. Once any header byte
/// arrives, the rest of the header and body must complete within `deadline`.
pub async fn read_frame_after_idle<R, T>(
    reader: &mut R,
    maximum: usize,
    deadline: Duration,
) -> Result<T>
where
    R: AsyncRead + Unpin,
    T: DeserializeOwned,
{
    let mut length = [0_u8; 4];
    reader
        .read_exact(&mut length[..1])
        .await
        .context("read protocol frame first length byte")?;
    tokio::time::timeout(deadline, reader.read_exact(&mut length[1..]))
        .await
        .context("protocol frame length read timed out")??;
    let length = u32::from_be_bytes(length) as usize;
    anyhow::ensure!(
        length <= maximum,
        "protocol frame declares {length} bytes; limit is {maximum}"
    );
    let mut bytes = vec![0_u8; length];
    tokio::time::timeout(deadline, reader.read_exact(&mut bytes))
        .await
        .context("protocol frame body read timed out")??;
    serde_json::from_slice(&bytes).context("decode protocol frame")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::UnixListener;

    #[tokio::test]
    async fn rejects_declared_length_before_allocating_body() -> Result<()> {
        let (mut writer, mut reader) = UnixStream::pair()?;
        writer
            .write_all(&u32::try_from(MAX_COMMAND_FRAME_BYTES + 1)?.to_be_bytes())
            .await?;
        let error = read_frame::<_, CommandReply>(
            &mut reader,
            MAX_COMMAND_FRAME_BYTES,
            Duration::from_secs(1),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("declares"));
        Ok(())
    }

    #[tokio::test]
    async fn one_path_accepts_independent_role_handshakes() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let path = temp.path().join("supervisor.sock");
        let listener = UnixListener::bind(&path)?;
        let server = tokio::spawn(async move {
            for expected in [ConnectionRole::Snapshots, ConnectionRole::Commands] {
                let (mut stream, _) = listener.accept().await.unwrap();
                let request: HandshakeRequest = read_frame(
                    &mut stream,
                    MAX_HANDSHAKE_FRAME_BYTES,
                    Duration::from_secs(1),
                )
                .await
                .unwrap();
                assert_eq!(request.role, expected);
                write_frame(
                    &mut stream,
                    &HandshakeReply {
                        protocol_version: SUPERVISOR_PROTOCOL_VERSION,
                        supervisor_generation: 7,
                        command_session: (expected == ConnectionRole::Commands)
                            .then_some(CommandSessionId([1; 16])),
                    },
                    MAX_HANDSHAKE_FRAME_BYTES,
                    Duration::from_secs(1),
                )
                .await
                .unwrap();
            }
        });
        let (_, snapshots) = connect_role(&path, ConnectionRole::Snapshots, None).await?;
        let (_, commands) = connect_role(&path, ConnectionRole::Commands, None).await?;
        assert!(snapshots.command_session.is_none());
        assert!(commands.command_session.is_some());
        server.await?;
        Ok(())
    }

    #[tokio::test]
    async fn lost_reply_invalidates_channel_and_never_reuses_sequence() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let path = temp.path().join("supervisor.sock");
        let listener = UnixListener::bind(&path)?;
        let server = tokio::spawn(async move {
            let (mut snapshots, _) = listener.accept().await.unwrap();
            let request: HandshakeRequest = read_frame(
                &mut snapshots,
                MAX_HANDSHAKE_FRAME_BYTES,
                Duration::from_secs(1),
            )
            .await
            .unwrap();
            assert_eq!(request.role, ConnectionRole::Snapshots);
            write_frame(
                &mut snapshots,
                &HandshakeReply {
                    protocol_version: SUPERVISOR_PROTOCOL_VERSION,
                    supervisor_generation: 7,
                    command_session: None,
                },
                MAX_HANDSHAKE_FRAME_BYTES,
                Duration::from_secs(1),
            )
            .await
            .unwrap();
            let snapshot = SupervisorSnapshotV0 {
                supervisor_generation: 7,
                ..SupervisorSnapshotV0::default()
            };
            write_frame(
                &mut snapshots,
                &snapshot,
                MAX_SNAPSHOT_FRAME_BYTES,
                Duration::from_secs(1),
            )
            .await
            .unwrap();

            let (mut commands, _) = listener.accept().await.unwrap();
            let request: HandshakeRequest = read_frame(
                &mut commands,
                MAX_HANDSHAKE_FRAME_BYTES,
                Duration::from_secs(1),
            )
            .await
            .unwrap();
            assert_eq!(request.role, ConnectionRole::Commands);
            write_frame(
                &mut commands,
                &HandshakeReply {
                    protocol_version: SUPERVISOR_PROTOCOL_VERSION,
                    supervisor_generation: 7,
                    command_session: Some(CommandSessionId([9; 16])),
                },
                MAX_HANDSHAKE_FRAME_BYTES,
                Duration::from_secs(1),
            )
            .await
            .unwrap();
            let request: CommandRequest = read_frame(
                &mut commands,
                MAX_COMMAND_FRAME_BYTES,
                Duration::from_secs(1),
            )
            .await
            .unwrap();
            assert_eq!(request.key.sequence, 1);
            // Drop without replying: execution is ambiguous to the client.
            drop(commands);
            std::future::pending::<()>().await;
        });

        let client = SupervisorClient::connect(&path).await?;
        let error = client.command(CommandAction::Shutdown).await.unwrap_err();
        assert!(error.to_string().contains("outcome is unknown"));
        {
            let channel = client.command.lock().await;
            assert_eq!(channel.next_sequence, 2);
            assert!(channel.stream.is_none());
        }
        let second = client.command(CommandAction::Shutdown).await.unwrap_err();
        assert!(second.to_string().contains("reconnect"));
        server.abort();
        Ok(())
    }

    #[tokio::test]
    async fn command_retry_uses_one_fresh_session_after_an_ambiguous_reply() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let path = temp.path().join("supervisor.sock");
        let listener = UnixListener::bind(&path)?;
        let server = tokio::spawn(async move {
            let (mut snapshots, _) = listener.accept().await.unwrap();
            let request: HandshakeRequest = read_frame(
                &mut snapshots,
                MAX_HANDSHAKE_FRAME_BYTES,
                Duration::from_secs(1),
            )
            .await
            .unwrap();
            assert_eq!(request.role, ConnectionRole::Snapshots);
            write_frame(
                &mut snapshots,
                &HandshakeReply {
                    protocol_version: SUPERVISOR_PROTOCOL_VERSION,
                    supervisor_generation: 7,
                    command_session: None,
                },
                MAX_HANDSHAKE_FRAME_BYTES,
                Duration::from_secs(1),
            )
            .await
            .unwrap();
            write_frame(
                &mut snapshots,
                &SupervisorSnapshotV0 {
                    supervisor_generation: 7,
                    ..SupervisorSnapshotV0::default()
                },
                MAX_SNAPSHOT_FRAME_BYTES,
                Duration::from_secs(1),
            )
            .await
            .unwrap();

            for (session, should_reply) in [
                (CommandSessionId([1; 16]), false),
                (CommandSessionId([2; 16]), true),
            ] {
                let (mut commands, _) = listener.accept().await.unwrap();
                let handshake: HandshakeRequest = read_frame(
                    &mut commands,
                    MAX_HANDSHAKE_FRAME_BYTES,
                    Duration::from_secs(1),
                )
                .await
                .unwrap();
                assert_eq!(handshake.role, ConnectionRole::Commands);
                assert!(handshake.resume_command_session.is_none());
                write_frame(
                    &mut commands,
                    &HandshakeReply {
                        protocol_version: SUPERVISOR_PROTOCOL_VERSION,
                        supervisor_generation: 7,
                        command_session: Some(session),
                    },
                    MAX_HANDSHAKE_FRAME_BYTES,
                    Duration::from_secs(1),
                )
                .await
                .unwrap();
                let request: CommandRequest = read_frame(
                    &mut commands,
                    MAX_COMMAND_FRAME_BYTES,
                    Duration::from_secs(1),
                )
                .await
                .unwrap();
                assert_eq!(request.action, CommandAction::Shutdown);
                assert_eq!(request.key.session, session);
                assert_eq!(request.key.sequence, 1);
                if should_reply {
                    write_frame(
                        &mut commands,
                        &CommandReply::accepted(),
                        MAX_COMMAND_FRAME_BYTES,
                        Duration::from_secs(1),
                    )
                    .await
                    .unwrap();
                }
            }
            drop(snapshots);
        });

        let client = SupervisorClient::connect(&path).await?;
        let reply = client
            .command_with_reconnect(CommandAction::Shutdown)
            .await?;
        assert!(reply.accepted);
        server.await?;
        Ok(())
    }

    #[tokio::test]
    async fn command_retry_stops_after_the_fresh_session_fails() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let path = temp.path().join("supervisor.sock");
        let listener = UnixListener::bind(&path)?;
        let (client_stream, mut first_server_stream) = UnixStream::pair()?;
        let (_, snapshot_rx) = watch::channel(SupervisorSnapshotV0::default());
        let (_, connection_rx) = watch::channel(ConnectionState::Connected);
        let client = SupervisorClient {
            path,
            snapshots: SnapshotStore {
                snapshots: snapshot_rx,
                connection: connection_rx,
            },
            command: Arc::new(Mutex::new(CommandChannel {
                stream: Some(client_stream),
                generation: 7,
                session: CommandSessionId([1; 16]),
                next_sequence: 1,
            })),
        };
        let server = tokio::spawn(async move {
            let first: CommandRequest = read_frame(
                &mut first_server_stream,
                MAX_COMMAND_FRAME_BYTES,
                Duration::from_secs(1),
            )
            .await
            .unwrap();
            assert_eq!(first.key.session, CommandSessionId([1; 16]));
            assert_eq!(first.key.sequence, 1);
            drop(first_server_stream);

            let (mut retry_stream, _) = listener.accept().await.unwrap();
            let handshake: HandshakeRequest = read_frame(
                &mut retry_stream,
                MAX_HANDSHAKE_FRAME_BYTES,
                Duration::from_secs(1),
            )
            .await
            .unwrap();
            assert_eq!(handshake.role, ConnectionRole::Commands);
            write_frame(
                &mut retry_stream,
                &HandshakeReply {
                    protocol_version: SUPERVISOR_PROTOCOL_VERSION,
                    supervisor_generation: 7,
                    command_session: Some(CommandSessionId([2; 16])),
                },
                MAX_HANDSHAKE_FRAME_BYTES,
                Duration::from_secs(1),
            )
            .await
            .unwrap();
            let retry: CommandRequest = read_frame(
                &mut retry_stream,
                MAX_COMMAND_FRAME_BYTES,
                Duration::from_secs(1),
            )
            .await
            .unwrap();
            assert_eq!(retry.key.session, CommandSessionId([2; 16]));
            assert_eq!(retry.key.sequence, 1);
            drop(retry_stream);

            assert!(
                tokio::time::timeout(Duration::from_millis(50), listener.accept())
                    .await
                    .is_err(),
                "a second reconnect would violate the one-retry boundary"
            );
        });

        let error = client
            .command_with_reconnect(CommandAction::Shutdown)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("retry supervisor command"));
        server.await?;
        Ok(())
    }
}
