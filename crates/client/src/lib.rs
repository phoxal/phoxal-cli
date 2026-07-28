//! Disposable client-side transport for the resident Phoxal supervisor.
//!
//! This crate owns supervisor connection and command transport. It must not own
//! resident process authority or terminal presentation; later extraction moves
//! retained client observation stores and source lifecycle here.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use std::{error::Error as StdError, fmt};

use anyhow::{Context, Result, bail};
use phoxal_cli_core::session::ProjectLifecycle;
use phoxal_cli_protocol::codec::async_io::{read_frame, read_frame_after_idle, write_frame};
use phoxal_cli_protocol::limits::{
    FRAME_READ_TIMEOUT, FRAME_WRITE_TIMEOUT, MAX_COMMAND_FRAME_BYTES, MAX_HANDSHAKE_FRAME_BYTES,
    MAX_SNAPSHOT_FRAME_BYTES,
};
use phoxal_cli_protocol::{
    CommandAction, CommandKey, CommandReply, CommandRequest, CommandSessionId, ConnectionRole,
    HandshakeReply, HandshakeRequest, SUPERVISOR_PROTOCOL_VERSION, SupervisorSnapshot,
    SupervisorSnapshotV0,
};
use tokio::net::UnixStream;
use tokio::sync::{Mutex, watch};

const RECONNECT_DELAY: Duration = Duration::from_millis(200);
const EARLY_CLOSE_RECOVERY: &str = concat!(
    "the resident rejected or disconnected during the supervisor handshake.\n\n",
    "It may have been started by an older Phoxal CLI, or it may be unhealthy.\n\n",
    "Stop it without using the supervisor protocol:\n\n",
    "    phoxal stop --force <target>\n\n",
    "Then start or attach again."
);

#[derive(Debug)]
struct ConnectionUnavailable {
    path: PathBuf,
    source: std::io::Error,
}

impl fmt::Display for ConnectionUnavailable {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "connect to unavailable supervisor {}: {}",
            self.path.display(),
            self.source
        )
    }
}

impl StdError for ConnectionUnavailable {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        Some(&self.source)
    }
}

/// Whether a connection attempt failed before any resident peer was reached.
///
/// Callers may retry this while a newly launched resident binds its socket.
/// Handshake, version, and snapshot failures are terminal and must be shown.
#[must_use]
pub fn is_connection_unavailable(error: &anyhow::Error) -> bool {
    error.downcast_ref::<ConnectionUnavailable>().is_some()
}

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
        let initial = read_frame::<_, SupervisorSnapshot>(
            &mut snapshot_stream,
            MAX_SNAPSHOT_FRAME_BYTES,
            FRAME_READ_TIMEOUT,
        )
        .await
        .context("read initial supervisor snapshot")?
        .into_v0();
        anyhow::ensure!(
            initial.supervisor_generation == snapshot_handshake.supervisor_generation,
            "snapshot generation does not match handshake"
        );
        let (snapshot_tx, snapshot_rx) = watch::channel(initial);
        let (connection_tx, connection_rx) = watch::channel(ConnectionState::Connected);
        let snapshot_task = tokio::spawn(snapshot_loop(
            path.clone(),
            snapshot_stream,
            snapshot_tx,
            connection_tx,
        ));

        let (command_stream, command_handshake) =
            match connect_role(&path, ConnectionRole::Commands, None).await {
                Ok(connected) => connected,
                Err(error) => {
                    snapshot_task.abort();
                    return Err(error);
                }
            };
        let session = match command_handshake.command_session {
            Some(session) => session,
            None => {
                snapshot_task.abort();
                bail!("command handshake did not issue a session")
            }
        };
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
                    ProjectLifecycle::Stopped | ProjectLifecycle::Failed
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
    read_frame_after_idle::<_, SupervisorSnapshot>(
        stream,
        MAX_SNAPSHOT_FRAME_BYTES,
        FRAME_READ_TIMEOUT,
    )
    .await
    .map(SupervisorSnapshot::into_v0)
}

pub async fn connect_role(
    path: &Path,
    role: ConnectionRole,
    resume_command_session: Option<CommandSessionId>,
) -> Result<(UnixStream, HandshakeReply)> {
    let mut stream = UnixStream::connect(path)
        .await
        .map_err(|source| ConnectionUnavailable {
            path: path.to_path_buf(),
            source,
        })?;
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
        read_frame(&mut stream, MAX_HANDSHAKE_FRAME_BYTES, FRAME_READ_TIMEOUT)
            .await
            .map_err(|_| anyhow::anyhow!(EARLY_CLOSE_RECOVERY))?;
    if reply.protocol_version != SUPERVISOR_PROTOCOL_VERSION {
        bail!(
            concat!(
                "the running resident uses supervisor protocol {}, but this CLI requires protocol {}.\n\n",
                "Stop the resident without using its protocol:\n\n",
                "    phoxal stop --force <target>\n\n",
                "Then start or attach again."
            ),
            reply.protocol_version,
            SUPERVISOR_PROTOCOL_VERSION
        );
    }
    Ok((stream, reply))
}

#[cfg(test)]
mod tests {
    use super::*;
    use phoxal_cli_protocol::{SupervisorSnapshot, SupervisorSnapshotV0};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn old_peer_early_close_reports_protocol_independent_recovery() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("supervisor.sock");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let peer = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            drop(stream);
        });

        let error = connect_role(&path, ConnectionRole::Snapshots, None)
            .await
            .unwrap_err();
        peer.await.unwrap();
        let message = format!("{error:#}");
        assert_eq!(message, EARLY_CLOSE_RECOVERY);
        assert!(
            message.contains("    phoxal stop --force <target>"),
            "{message}"
        );
    }

    #[tokio::test]
    async fn nonexistent_socket_is_classified_as_retryable_unavailable() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("missing.sock");
        let error = connect_role(&path, ConnectionRole::Snapshots, None)
            .await
            .unwrap_err();
        assert!(is_connection_unavailable(&error), "{error:#}");
    }

    #[tokio::test]
    async fn decoded_remote_version_is_reported_exactly() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("supervisor.sock");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let peer = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let _: HandshakeRequest =
                read_frame(&mut stream, MAX_HANDSHAKE_FRAME_BYTES, FRAME_READ_TIMEOUT)
                    .await
                    .unwrap();
            write_frame(
                &mut stream,
                &HandshakeReply {
                    protocol_version: 7,
                    supervisor_generation: 1,
                    command_session: None,
                },
                MAX_HANDSHAKE_FRAME_BYTES,
                FRAME_WRITE_TIMEOUT,
            )
            .await
            .unwrap();
        });

        let error = connect_role(&path, ConnectionRole::Snapshots, None)
            .await
            .unwrap_err();
        peer.await.unwrap();
        let message = format!("{error:#}");
        assert!(
            message.contains(
                "running resident uses supervisor protocol 7, but this CLI requires protocol 1"
            ),
            "{message}"
        );
        assert!(
            message.contains("    phoxal stop --force <target>"),
            "{message}"
        );
    }

    #[tokio::test]
    async fn failed_command_handshake_aborts_the_started_snapshot_task() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("supervisor.sock");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let peer = tokio::spawn(async move {
            let (mut snapshots, _) = listener.accept().await.unwrap();
            let _: HandshakeRequest = read_frame(
                &mut snapshots,
                MAX_HANDSHAKE_FRAME_BYTES,
                FRAME_READ_TIMEOUT,
            )
            .await
            .unwrap();
            write_frame(
                &mut snapshots,
                &HandshakeReply {
                    protocol_version: SUPERVISOR_PROTOCOL_VERSION,
                    supervisor_generation: 1,
                    command_session: None,
                },
                MAX_HANDSHAKE_FRAME_BYTES,
                FRAME_WRITE_TIMEOUT,
            )
            .await
            .unwrap();
            let snapshot = SupervisorSnapshotV0 {
                supervisor_generation: 1,
                ..SupervisorSnapshotV0::default()
            };
            write_frame(
                &mut snapshots,
                &SupervisorSnapshot::V0(snapshot),
                MAX_SNAPSHOT_FRAME_BYTES,
                FRAME_WRITE_TIMEOUT,
            )
            .await
            .unwrap();

            let (mut commands, _) = listener.accept().await.unwrap();
            let _: HandshakeRequest =
                read_frame(&mut commands, MAX_HANDSHAKE_FRAME_BYTES, FRAME_READ_TIMEOUT)
                    .await
                    .unwrap();
            commands.shutdown().await.unwrap();
            drop(commands);

            let mut byte = [0_u8; 1];
            tokio::time::timeout(Duration::from_secs(1), snapshots.read(&mut byte))
                .await
                .expect("snapshot connection should close when command handshake fails")
                .unwrap()
        });

        let error = match SupervisorClient::connect(&path).await {
            Ok(_) => panic!("failed command handshake unexpectedly connected"),
            Err(error) => error,
        };
        assert_eq!(format!("{error:#}"), EARLY_CLOSE_RECOVERY);
        assert_eq!(peer.await.unwrap(), 0);
    }
}
