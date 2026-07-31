//! Resident Unix socket, snapshot serving, and command dispatch.

use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use phoxal_cli_protocol::codec::async_io::{read_frame, read_frame_after_idle, write_frame};
use phoxal_cli_protocol::limits::{
    FRAME_READ_TIMEOUT, FRAME_WRITE_TIMEOUT, MAX_COMMAND_FRAME_BYTES, MAX_HANDSHAKE_FRAME_BYTES,
    MAX_SNAPSHOT_FRAME_BYTES,
};
use phoxal_cli_protocol::{
    CommandRequest, CommandSessionId, ConnectionRole, HandshakeReply, HandshakeRequest,
    SUPERVISOR_PROTOCOL_VERSION, SupervisorSnapshot, validate_snapshot_bounds,
};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::{SupervisorAction, SupervisorState};

use super::command_sessions::{CommandSessions, issue_or_resume_session, process_command};

#[derive(Clone)]
pub(super) struct ServerState {
    pub(super) board: SupervisorState,
    pub(super) actions: mpsc::Sender<SupervisorAction>,
    pub(super) supervisor_token: CancellationToken,
    pub(super) sessions: Arc<Mutex<CommandSessions>>,
    /// Every accepted connection is spawned onto this tracker from the
    /// moment `accept_loop` dequeues it - handshake included - so there is
    /// no window where an accepted connection is invisible to
    /// `ResidentSocket::close`. A command connection can stay open
    /// indefinitely, so once `handle_connection` classifies one it hands
    /// off to an untracked task and returns, freeing its tracked slot; a
    /// snapshot connection never hands off, since its own completion (a
    /// terminal frame written, or a connection error) is exactly the signal
    /// `close` needs. See `handle_connection` and `ResidentSocket::close`'s
    /// doc comment.
    pub(super) connection_tracker: TaskTracker,
}

impl ServerState {
    pub(super) fn new(
        board: SupervisorState,
        actions: mpsc::Sender<SupervisorAction>,
        supervisor_token: CancellationToken,
        connection_tracker: TaskTracker,
    ) -> Self {
        Self {
            board,
            actions,
            supervisor_token,
            sessions: Arc::default(),
            connection_tracker,
        }
    }
}

pub(super) async fn accept_loop(
    listener: UnixListener,
    state: ServerState,
    stop: CancellationToken,
) {
    loop {
        tokio::select! {
            _ = stop.cancelled() => return,
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => {
                    let state = state.clone();
                    // Cloned before `state` moves into the spawned future,
                    // and spawned on the tracker (not a bare `tokio::spawn`)
                    // so this connection is tracked synchronously, in the
                    // same loop iteration as its acceptance - no await
                    // separates "accepted" from "tracked".
                    let tracker = state.connection_tracker.clone();
                    tracker.spawn(async move {
                        if let Err(error) = handle_connection(stream, state).await {
                            tracing::debug!(error = %error, "supervisor client disconnected");
                        }
                    });
                }
                Err(error) => {
                    tracing::warn!(error = %error, "supervisor socket accept failed");
                    return;
                }
            }
        }
    }
}

async fn handle_connection(mut stream: UnixStream, state: ServerState) -> Result<()> {
    let request: HandshakeRequest =
        read_frame(&mut stream, MAX_HANDSHAKE_FRAME_BYTES, FRAME_READ_TIMEOUT)
            .await
            .context("read supervisor handshake")?;
    let generation = state.board.supervisor_snapshot().supervisor_generation;
    if request.protocol_version != SUPERVISOR_PROTOCOL_VERSION {
        write_frame(
            &mut stream,
            &HandshakeReply {
                protocol_version: SUPERVISOR_PROTOCOL_VERSION,
                supervisor_generation: generation,
                command_session: None,
            },
            MAX_HANDSHAKE_FRAME_BYTES,
            FRAME_WRITE_TIMEOUT,
        )
        .await?;
        bail!(
            "unsupported supervisor protocol version {}",
            request.protocol_version
        );
    }
    let session = matches!(request.role, ConnectionRole::Commands)
        .then(|| issue_or_resume_session(&state.sessions, request.resume_command_session));
    write_frame(
        &mut stream,
        &HandshakeReply {
            protocol_version: SUPERVISOR_PROTOCOL_VERSION,
            supervisor_generation: generation,
            command_session: session,
        },
        MAX_HANDSHAKE_FRAME_BYTES,
        FRAME_WRITE_TIMEOUT,
    )
    .await?;
    match request.role {
        // Already running on `connection_tracker` (see `accept_loop`), and
        // stays there for its whole life: `serve_snapshots` only returns
        // after writing a terminal frame or hitting a connection error, so
        // the task's exit IS delivery completion (or a definitive reason it
        // can never complete).
        ConnectionRole::Snapshots => serve_snapshots(stream, state.board).await,
        ConnectionRole::Commands => {
            // Classified as long-lived: hand off to an untracked task now,
            // so it can stay open indefinitely without holding
            // `ResidentSocket::close`'s bounded wait open. The handshake
            // above already ran on the tracker, so there was no window
            // where this connection was invisible to `close`.
            let session = session.expect("command role issued a session");
            tokio::spawn(async move {
                if let Err(error) = serve_commands(stream, state, session).await {
                    tracing::debug!(error = %error, "supervisor client disconnected");
                }
            });
            Ok(())
        }
    }
}

async fn serve_snapshots(mut stream: UnixStream, board: SupervisorState) -> Result<()> {
    let mut snapshots = board.subscribe();
    loop {
        let snapshot = snapshots.borrow_and_update().clone();
        validate_snapshot_bounds(&snapshot)
            .context("supervisor produced an out-of-bounds snapshot")?;
        let terminal = matches!(
            snapshot.lifecycle,
            phoxal_cli_core::runtime::ProjectLifecycle::Stopped
                | phoxal_cli_core::runtime::ProjectLifecycle::Failed
        );
        write_frame(
            &mut stream,
            &SupervisorSnapshot::V0(snapshot),
            MAX_SNAPSHOT_FRAME_BYTES,
            FRAME_WRITE_TIMEOUT,
        )
        .await?;
        if terminal {
            return Ok(());
        }
        snapshots
            .changed()
            .await
            .context("supervisor snapshot publisher closed")?;
    }
}

async fn serve_commands(
    mut stream: UnixStream,
    state: ServerState,
    session: CommandSessionId,
) -> Result<()> {
    loop {
        let request: CommandRequest =
            read_frame_after_idle(&mut stream, MAX_COMMAND_FRAME_BYTES, FRAME_READ_TIMEOUT).await?;
        let reply = process_command(&state, session, request);
        write_frame(
            &mut stream,
            &reply,
            MAX_COMMAND_FRAME_BYTES,
            FRAME_WRITE_TIMEOUT,
        )
        .await?;
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    /// A deterministic producer identity for tests, so a case can name the
    /// exact restart it means.
    fn producer(seed: u8) -> ProducerId {
        ProducerId::parse(&format!("{:032x}", u128::from(seed)))
            .expect("test producer id must parse")
    }
    use super::super::command_sessions::COMMAND_SESSION_TTL;
    use super::*;
    use phoxal_cli_core::identity::ProducerId;
    use phoxal_cli_core::runtime::{ParticipantKind, ProcessKey, ProcessState, StartupRequirement};
    use phoxal_cli_protocol::limits::MAX_RECENT_COMMAND_REPLIES;
    use phoxal_cli_protocol::{CommandAction, CommandError};

    #[test]
    fn command_watermark_replays_and_fences_producers() {
        let board = SupervisorState::new();
        let key = ProcessKey::project("worker");
        board.upsert_process(
            key.clone(),
            ParticipantKind::Service,
            ProcessState::Ready,
            StartupRequirement::Required,
        );
        board.set_producer(&key, producer(7));
        let (actions, mut action_rx) = mpsc::channel(4);
        let token = CancellationToken::new();
        let state = ServerState {
            board: board.clone(),
            actions,
            supervisor_token: token,
            sessions: Arc::default(),
            connection_tracker: TaskTracker::new(),
        };
        let session = issue_or_resume_session(&state.sessions, None);
        let request = |sequence, restart_of| CommandRequest {
            supervisor_generation: board.supervisor_snapshot().supervisor_generation,
            key: phoxal_cli_protocol::CommandKey { session, sequence },
            action: CommandAction::Restart {
                process: key.clone(),
                expected_producer: producer(restart_of),
            },
        };
        assert!(process_command(&state, session, request(1, 7)).accepted);
        assert!(process_command(&state, session, request(1, 7)).accepted);
        assert!(action_rx.try_recv().is_ok());
        assert!(
            action_rx.try_recv().is_err(),
            "retry must not execute twice"
        );
        assert_eq!(
            process_command(&state, session, request(3, 7)).error,
            Some(CommandError::OutOfOrder)
        );
        assert_eq!(
            process_command(&state, session, request(2, 8)).error,
            Some(CommandError::SupersededProducer)
        );
    }

    #[test]
    fn pending_restart_is_deduplicated_across_fresh_sessions_until_the_producer_advances() {
        let board = SupervisorState::new();
        let key = ProcessKey::project("worker");
        board.upsert_process(
            key.clone(),
            ParticipantKind::Service,
            ProcessState::Ready,
            StartupRequirement::Required,
        );
        board.set_producer(&key, producer(7));
        let (actions, mut action_rx) = mpsc::channel(4);
        let state = ServerState {
            board: board.clone(),
            actions,
            supervisor_token: CancellationToken::new(),
            sessions: Arc::default(),
            connection_tracker: TaskTracker::new(),
        };
        let request = |session, restart_of| CommandRequest {
            supervisor_generation: board.supervisor_snapshot().supervisor_generation,
            key: phoxal_cli_protocol::CommandKey {
                session,
                sequence: 1,
            },
            action: CommandAction::Restart {
                process: key.clone(),
                expected_producer: producer(restart_of),
            },
        };

        let first_session = issue_or_resume_session(&state.sessions, None);
        assert!(process_command(&state, first_session, request(first_session, 7)).accepted);
        let retry_session = issue_or_resume_session(&state.sessions, None);
        let retry = process_command(&state, retry_session, request(retry_session, 7));
        assert_eq!(retry.error, Some(CommandError::AlreadyProcessed));
        assert!(matches!(
            action_rx.try_recv(),
            Ok(SupervisorAction::Restart { key: queued }) if queued == key
        ));
        assert!(
            action_rx.try_recv().is_err(),
            "fresh-session retry must not enqueue a duplicate restart"
        );

        board.set_producer(&key, producer(8));
        let next_session = issue_or_resume_session(&state.sessions, None);
        assert!(process_command(&state, next_session, request(next_session, 8)).accepted);
        assert!(matches!(
            action_rx.try_recv(),
            Ok(SupervisorAction::Restart { key: queued }) if queued == key
        ));
    }

    #[test]
    fn evicted_replies_and_expired_sessions_never_execute_again() {
        let board = SupervisorState::new();
        let (actions, _action_rx) = mpsc::channel(1);
        let state = ServerState {
            board: board.clone(),
            actions,
            supervisor_token: CancellationToken::new(),
            sessions: Arc::default(),
            connection_tracker: TaskTracker::new(),
        };
        let session = issue_or_resume_session(&state.sessions, None);
        let generation = board.supervisor_snapshot().supervisor_generation;
        for sequence in 1..=(MAX_RECENT_COMMAND_REPLIES as u64 + 1) {
            let reply = process_command(
                &state,
                session,
                CommandRequest {
                    supervisor_generation: generation,
                    key: phoxal_cli_protocol::CommandKey { session, sequence },
                    action: CommandAction::Shutdown,
                },
            );
            assert!(reply.accepted);
        }
        let old = process_command(
            &state,
            session,
            CommandRequest {
                supervisor_generation: generation,
                key: phoxal_cli_protocol::CommandKey {
                    session,
                    sequence: 1,
                },
                action: CommandAction::Shutdown,
            },
        );
        assert_eq!(old.error, Some(CommandError::AlreadyProcessed));

        state
            .sessions
            .lock()
            .unwrap()
            .active
            .get_mut(&session)
            .unwrap()
            .last_used = Instant::now() - COMMAND_SESSION_TTL;
        let replacement = issue_or_resume_session(&state.sessions, Some(session));
        assert_ne!(replacement, session);
        let expired = process_command(
            &state,
            session,
            CommandRequest {
                supervisor_generation: generation,
                key: phoxal_cli_protocol::CommandKey {
                    session,
                    sequence: MAX_RECENT_COMMAND_REPLIES as u64 + 2,
                },
                action: CommandAction::Shutdown,
            },
        );
        assert_eq!(expired.error, Some(CommandError::InvalidSession));
    }

    #[tokio::test]
    async fn incompatible_handshake_receives_server_version_before_rejection() {
        let board = SupervisorState::new();
        let (actions, _action_rx) = mpsc::channel(1);
        let state = ServerState {
            board: board.clone(),
            actions,
            supervisor_token: CancellationToken::new(),
            sessions: Arc::default(),
            connection_tracker: TaskTracker::new(),
        };
        let healthy_session = issue_or_resume_session(&state.sessions, None);
        let (mut client, server) = UnixStream::pair().unwrap();
        let server_task = tokio::spawn(handle_connection(server, state.clone()));

        write_frame(
            &mut client,
            &HandshakeRequest {
                protocol_version: 1,
                role: ConnectionRole::Commands,
                resume_command_session: None,
            },
            MAX_HANDSHAKE_FRAME_BYTES,
            FRAME_WRITE_TIMEOUT,
        )
        .await
        .unwrap();
        let reply: HandshakeReply =
            read_frame(&mut client, MAX_HANDSHAKE_FRAME_BYTES, FRAME_READ_TIMEOUT)
                .await
                .unwrap();
        assert_eq!(reply.protocol_version, SUPERVISOR_PROTOCOL_VERSION);
        assert_eq!(
            reply.supervisor_generation,
            board.supervisor_snapshot().supervisor_generation
        );
        assert!(reply.command_session.is_none());
        assert!(
            server_task
                .await
                .unwrap()
                .unwrap_err()
                .to_string()
                .contains("unsupported supervisor protocol version 1")
        );
        let sessions = state
            .sessions
            .lock()
            .expect("command sessions mutex poisoned");
        assert_eq!(sessions.active.len(), 1);
        assert!(sessions.active.contains_key(&healthy_session));
    }
}
