use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio::net::UnixListener;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::{SupervisorAction, SupervisorState};

use super::server::{ServerState, accept_loop};

/// Upper bound on how long `close` waits for the final snapshot revision to
/// reach every currently connected client. Bounded rather than unconditional
/// because a client that stops reading (or a peer that never existed) must
/// never wedge resident shutdown.
const CLOSE_DELIVERY_TIMEOUT: Duration = Duration::from_secs(1);

pub struct ResidentSocket {
    path: PathBuf,
    stop: CancellationToken,
    accept_task: Option<tokio::task::JoinHandle<()>>,
    connection_tracker: TaskTracker,
}

impl ResidentSocket {
    /// Bind while the caller holds the project lock. Only this authority may
    /// remove a stale pathname.
    pub fn bind(
        project: &Path,
        board: SupervisorState,
        actions: mpsc::Sender<SupervisorAction>,
        supervisor_token: CancellationToken,
    ) -> Result<Self> {
        let path = supervisor_socket_path(project)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to remove stale {}", path.display()));
            }
        }
        let listener = UnixListener::bind(&path)
            .with_context(|| format!("failed to bind {}", path.display()))?;
        let stop = CancellationToken::new();
        let connection_tracker = TaskTracker::new();
        let state = ServerState::new(board, actions, supervisor_token, connection_tracker.clone());
        let accept_task = tokio::spawn(accept_loop(listener, state, stop.clone()));
        Ok(Self {
            path,
            stop,
            accept_task: Some(accept_task),
            connection_tracker,
        })
    }

    /// Stop accepting new connections and wait for the accept loop itself to
    /// finish, THEN close `connection_tracker` and wait - bounded by
    /// [`CLOSE_DELIVERY_TIMEOUT`] - for every already-accepted connection to
    /// finish, before removing the pathname while lock authority is held.
    ///
    /// This ordering is load-bearing: every accepted connection is spawned
    /// onto `connection_tracker` synchronously, in the same `accept_loop`
    /// iteration that accepts it (see server.rs), so by the time
    /// `accept_task` has actually returned, the tracked set is complete by
    /// construction - closing and waiting on the tracker only afterward
    /// means there is no connection that was accepted but not yet
    /// registered when `wait` starts, which a "close-then-wait-then-await
    /// the accept loop" ordering could otherwise miss.
    ///
    /// Completion is "every tracked task has exited", which for a snapshot
    /// connection (`serve_snapshots` in server.rs) only happens after it
    /// writes a terminal (`Stopped`/`Failed`) frame and returns, or the
    /// connection itself failed - either way there is nothing further to
    /// deliver to that client. A command connection hands itself off to an
    /// untracked task as soon as it is classified (it can stay open
    /// indefinitely), so it never holds this wait to its bound.
    pub async fn close(mut self) {
        self.stop.cancel();
        if let Some(accept_task) = self.accept_task.take() {
            let _ = accept_task.await;
        }
        self.connection_tracker.close();
        let _ = tokio::time::timeout(CLOSE_DELIVERY_TIMEOUT, self.connection_tracker.wait()).await;
        let _ = std::fs::remove_file(&self.path);
    }
}

impl Drop for ResidentSocket {
    fn drop(&mut self) {
        // Created only after project-lock acquisition and dropped before that
        // lock guard, including error unwinds.
        self.stop.cancel();
        let _ = std::fs::remove_file(&self.path);
    }
}

pub fn supervisor_socket_path(project: &Path) -> Result<PathBuf> {
    let path = phoxal_cli_core::runtime::paths::RuntimePaths::for_root(project).supervisor_socket();
    let absolute = if path.is_absolute() {
        path
    } else {
        std::env::current_dir()?.join(path)
    };
    let bytes = OsStr::new(&absolute).as_bytes();
    let maximum =
        std::mem::size_of::<libc::sockaddr_un>() - std::mem::size_of::<libc::sa_family_t>() - 1;
    if bytes.len() > maximum {
        bail!(
            "project supervisor socket path is {} bytes but this platform supports at most {maximum}: {}; move the project to a shorter path",
            bytes.len(),
            absolute.display()
        );
    }
    Ok(absolute)
}

#[cfg(test)]
mod tests {
    use super::*;
    use phoxal_cli_protocol::codec::async_io::{read_frame, write_frame};
    use phoxal_cli_protocol::limits::{
        FRAME_READ_TIMEOUT, FRAME_WRITE_TIMEOUT, MAX_HANDSHAKE_FRAME_BYTES,
        MAX_SNAPSHOT_FRAME_BYTES,
    };
    use phoxal_cli_protocol::{
        ConnectionRole, HandshakeReply, HandshakeRequest, SUPERVISOR_PROTOCOL_VERSION,
        SupervisorSnapshot,
    };

    #[test]
    fn socket_path_is_project_local_and_rejects_overlong_paths() {
        let project = Path::new("/tmp/phoxal-project");
        assert_eq!(
            supervisor_socket_path(project).unwrap(),
            project.join(".phoxal/run/supervisor.sock")
        );
        let long = PathBuf::from(format!("/tmp/{}", "x".repeat(256)));
        assert!(
            supervisor_socket_path(&long)
                .unwrap_err()
                .to_string()
                .contains("shorter")
        );
    }

    /// End-to-end proof for the close/delivery blocker: a connected
    /// snapshot client must receive the terminal (`Failed`) revision -
    /// before `close()` returns and removes the pathname - well under the
    /// bounded timeout, not by luck from a blind sleep.
    #[tokio::test]
    async fn close_delivers_the_terminal_snapshot_before_removing_the_socket() {
        let project = tempfile::tempdir().expect("temp project dir");
        let board = SupervisorState::new();
        let (actions, _action_rx) = mpsc::channel(4);
        let socket = ResidentSocket::bind(
            project.path(),
            board.clone(),
            actions,
            CancellationToken::new(),
        )
        .expect("bind resident socket");
        let path = supervisor_socket_path(project.path()).expect("socket path");

        let mut client = tokio::net::UnixStream::connect(&path)
            .await
            .expect("connect to resident socket");
        write_frame(
            &mut client,
            &HandshakeRequest {
                protocol_version: SUPERVISOR_PROTOCOL_VERSION,
                role: ConnectionRole::Snapshots,
                resume_command_session: None,
            },
            MAX_HANDSHAKE_FRAME_BYTES,
            FRAME_WRITE_TIMEOUT,
        )
        .await
        .expect("write handshake");
        let _reply: HandshakeReply =
            read_frame(&mut client, MAX_HANDSHAKE_FRAME_BYTES, FRAME_READ_TIMEOUT)
                .await
                .expect("read handshake reply");
        // Reading this confirms the tracked `serve_snapshots` task is
        // already registered in `connection_tracker` before `close` runs.
        let initial: SupervisorSnapshot =
            read_frame(&mut client, MAX_SNAPSHOT_FRAME_BYTES, FRAME_READ_TIMEOUT)
                .await
                .expect("read initial snapshot");
        assert_eq!(
            initial.as_v0().lifecycle,
            phoxal_cli_core::runtime::ProjectLifecycle::Starting
        );

        board.fail("boom: catalog train floor not supported");

        let close_started = std::time::Instant::now();
        socket.close().await;
        assert!(
            close_started.elapsed() < CLOSE_DELIVERY_TIMEOUT,
            "close took {:?}; the tracked task should finish almost immediately \
             once it observes the board change, not by exhausting the bound",
            close_started.elapsed()
        );
        assert!(!path.exists(), "close must remove the socket pathname");

        let terminal: SupervisorSnapshot =
            read_frame(&mut client, MAX_SNAPSHOT_FRAME_BYTES, FRAME_READ_TIMEOUT)
                .await
                .expect("terminal snapshot must already be in the socket buffer");
        let terminal = terminal.into_v0();
        assert_eq!(
            terminal.lifecycle,
            phoxal_cli_core::runtime::ProjectLifecycle::Failed
        );
        assert_eq!(
            terminal.failure.as_deref(),
            Some("boom: catalog train floor not supported")
        );
    }

    /// Deterministic proof for the late-registration race: a connection
    /// that has been ACCEPTED but has not yet exchanged a single handshake
    /// byte must already be tracked, and `close` (already running
    /// concurrently) must not return before that connection finishes its
    /// handshake and delivers the terminal snapshot.
    ///
    /// The OS-level moment a client's `connect()` resolves does not
    /// deterministically imply the server's `accept_loop` has already
    /// dequeued and tracked it - that depends on runtime scheduling, which
    /// this test does not control. What IS controlled and asserted
    /// directly: `connection_tracker.is_empty()` is polled (bounded, via
    /// `yield_now`) until it goes false, which is a real observation of
    /// `accept_loop` having tracked the connection, not a timing guess. By
    /// construction (see `accept_loop`, which never awaits between
    /// accepting a connection and spawning it onto the tracker), that
    /// moment is always before the handshake starts, since the handshake
    /// only begins after this test explicitly writes it - below.
    #[tokio::test]
    async fn close_waits_for_a_connection_accepted_before_its_handshake_even_starts() {
        let project = tempfile::tempdir().expect("temp project dir");
        let board = SupervisorState::new();
        let (actions, _action_rx) = mpsc::channel(4);
        let socket = ResidentSocket::bind(
            project.path(),
            board.clone(),
            actions,
            CancellationToken::new(),
        )
        .expect("bind resident socket");
        let path = supervisor_socket_path(project.path()).expect("socket path");

        // Connected, but nothing has been written or read yet - the
        // supervisor protocol handshake has not started.
        let mut client = tokio::net::UnixStream::connect(&path)
            .await
            .expect("connect to resident socket");

        let mut tracked = false;
        for _ in 0..10_000 {
            if !socket.connection_tracker.is_empty() {
                tracked = true;
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(
            tracked,
            "connection must be tracked as soon as accept_loop dequeues it, \
             before its handshake starts"
        );

        // `close` starts concurrently, before this connection has done
        // anything beyond connecting.
        let close_started = std::time::Instant::now();
        let close_task = tokio::spawn(socket.close());

        write_frame(
            &mut client,
            &HandshakeRequest {
                protocol_version: SUPERVISOR_PROTOCOL_VERSION,
                role: ConnectionRole::Snapshots,
                resume_command_session: None,
            },
            MAX_HANDSHAKE_FRAME_BYTES,
            FRAME_WRITE_TIMEOUT,
        )
        .await
        .expect("write handshake");
        let _reply: HandshakeReply =
            read_frame(&mut client, MAX_HANDSHAKE_FRAME_BYTES, FRAME_READ_TIMEOUT)
                .await
                .expect("read handshake reply");
        let initial: SupervisorSnapshot =
            read_frame(&mut client, MAX_SNAPSHOT_FRAME_BYTES, FRAME_READ_TIMEOUT)
                .await
                .expect("read initial snapshot");
        assert_eq!(
            initial.as_v0().lifecycle,
            phoxal_cli_core::runtime::ProjectLifecycle::Starting
        );

        board.fail("boom: accepted before its handshake started");

        let terminal: SupervisorSnapshot =
            read_frame(&mut client, MAX_SNAPSHOT_FRAME_BYTES, FRAME_READ_TIMEOUT)
                .await
                .expect(
                    "terminal snapshot must be delivered even though close() started \
                     before this connection's handshake did",
                );
        let terminal = terminal.into_v0();
        assert_eq!(
            terminal.lifecycle,
            phoxal_cli_core::runtime::ProjectLifecycle::Failed
        );
        assert_eq!(
            terminal.failure.as_deref(),
            Some("boom: accepted before its handshake started")
        );

        close_task.await.expect("close task must not panic");
        assert!(
            close_started.elapsed() < CLOSE_DELIVERY_TIMEOUT,
            "close took {:?}",
            close_started.elapsed()
        );
        assert!(!path.exists(), "close must remove the socket pathname");
    }
}
