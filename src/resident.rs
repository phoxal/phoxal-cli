//! Project-local resident-supervisor transport.

use std::collections::{BTreeMap, HashMap};
use std::ffi::OsStr;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use phoxal_cli_client::{
    FRAME_READ_TIMEOUT, FRAME_WRITE_TIMEOUT, read_frame, read_frame_after_idle, write_frame,
};
use phoxal_cli_core::session::protocol::{
    MAX_COMMAND_FRAME_BYTES, MAX_HANDSHAKE_FRAME_BYTES, MAX_RECENT_COMMAND_REPLIES,
    MAX_SNAPSHOT_FRAME_BYTES, SUPERVISOR_PROTOCOL_VERSION,
};
use phoxal_cli_core::session::{
    BootstrapResult, CommandAction, CommandError, CommandReply, CommandRequest, CommandSessionId,
    ConnectionRole, HandshakeReply, HandshakeRequest, LaunchNonce,
};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::supervisor::{BoardBackend, SupervisorAction};

const COMMAND_SESSION_TTL: Duration = Duration::from_secs(5 * 60);
const MAX_COMMAND_SESSIONS: usize = 64;
const BOOTSTRAP_FD_ENV: &str = "PHOXAL_RESIDENT_BOOTSTRAP_FD";
const BOOTSTRAP_NONCE_ENV: &str = "PHOXAL_RESIDENT_LAUNCH_NONCE";
const RESIDENT_LOG_MAX_BYTES: u64 = 2 * 1024 * 1024;
const RESIDENT_LOG_ROTATIONS: usize = 3;
static BOOTSTRAP_REPORTED: AtomicBool = AtomicBool::new(false);

#[derive(Debug)]
struct CommandSessionState {
    highest_processed: u64,
    recent_replies: BTreeMap<u64, CommandReply>,
    last_used: Instant,
}

#[derive(Debug, Default)]
struct CommandSessions {
    active: HashMap<CommandSessionId, CommandSessionState>,
    /// Restart acceptance must be fenced across command sessions. A client
    /// may reconnect after losing a reply, before the supervisor consumes the
    /// first action and advances the board incarnation.
    pending_restarts: HashMap<phoxal_cli_core::session::ProcessKey, u64>,
}

#[derive(Clone)]
struct ServerState {
    board: BoardBackend,
    actions: mpsc::Sender<SupervisorAction>,
    supervisor_token: CancellationToken,
    sessions: Arc<Mutex<CommandSessions>>,
}

pub struct ResidentSocket {
    path: PathBuf,
    stop: CancellationToken,
    accept_task: Option<tokio::task::JoinHandle<()>>,
}

pub struct LaunchedResident {
    pub child: Child,
    pub result: BootstrapResult,
}

#[must_use]
pub fn has_private_bootstrap() -> bool {
    std::env::var_os(BOOTSTRAP_FD_ENV).is_some()
}

pub fn launch_detached() -> Result<LaunchedResident> {
    let (mut parent, child_socket) =
        std::os::unix::net::UnixStream::pair().context("create resident bootstrap socketpair")?;
    parent.set_read_timeout(Some(Duration::from_secs(30)))?;
    let child_fd = child_socket.as_raw_fd();
    let nonce = random_launch_nonce();
    let log = resident_log_file()?;
    let stderr = log.try_clone().context("clone resident log descriptor")?;
    let mut command = Command::new(std::env::current_exe()?);
    command
        .args(std::env::args_os().skip(1))
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(stderr))
        .env(BOOTSTRAP_FD_ENV, child_fd.to_string())
        .env(BOOTSTRAP_NONCE_ENV, hex::encode(nonce.0));
    // SAFETY: runs in the post-fork child before exec. It only invokes
    // async-signal-safe libc calls and reports failures through Command.
    unsafe {
        command.pre_exec(move || {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            let flags = libc::fcntl(child_fd, libc::F_GETFD);
            if flags == -1 || libc::fcntl(child_fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) == -1
            {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = command.spawn().context("spawn resident supervisor")?;
    drop(child_socket);
    let result: BootstrapResult = read_sync_frame(
        &mut parent,
        phoxal_cli_core::session::protocol::MAX_HANDSHAKE_FRAME_BYTES,
    )
    .context("resident did not complete private bootstrap")?;
    match &result {
        BootstrapResult::Bound { launch_nonce, .. } if launch_nonce == &nonce => {}
        BootstrapResult::Bound { .. } => bail!("resident bootstrap nonce did not match launcher"),
        BootstrapResult::Rejected { .. } => {}
    }
    Ok(LaunchedResident { child, result })
}

pub fn private_bootstrap_nonce() -> Result<Option<LaunchNonce>> {
    let Some(value) = std::env::var_os(BOOTSTRAP_NONCE_ENV) else {
        return Ok(None);
    };
    let bytes = hex::decode(value.to_string_lossy().as_bytes())
        .context("invalid private resident launch nonce")?;
    anyhow::ensure!(
        bytes.len() == 32,
        "private resident launch nonce has invalid length"
    );
    let mut nonce = [0_u8; 32];
    nonce.copy_from_slice(&bytes);
    Ok(Some(LaunchNonce(nonce)))
}

pub fn report_private_bootstrap(result: &BootstrapResult) -> Result<()> {
    if BOOTSTRAP_REPORTED.swap(true, Ordering::SeqCst) {
        return Ok(());
    }
    let Some(fd) = std::env::var(BOOTSTRAP_FD_ENV)
        .ok()
        .and_then(|value| value.parse::<i32>().ok())
    else {
        return Ok(());
    };
    // SAFETY: this descriptor was supplied by our launcher for this one-shot
    // bootstrap and ownership transfers to this File exactly once.
    let mut socket = unsafe { File::from_raw_fd(fd) };
    write_sync_frame(
        &mut socket,
        result,
        phoxal_cli_core::session::protocol::MAX_HANDSHAKE_FRAME_BYTES,
    )
}

fn random_launch_nonce() -> LaunchNonce {
    let mut bytes = [0_u8; 32];
    getrandom::fill(&mut bytes).expect("operating-system CSPRNG unavailable");
    LaunchNonce(bytes)
}

fn resident_log_file() -> Result<File> {
    let project = std::env::var_os(crate::host_paths::PROJECT_ROOT_ENV)
        .map(PathBuf::from)
        .unwrap_or(std::env::current_dir()?);
    let path = crate::runtime_paths::RuntimePaths::for_root(&project).supervisor_log();
    let directory = path
        .parent()
        .expect("supervisor log has a parent")
        .to_path_buf();
    std::fs::create_dir_all(&directory)?;
    if path
        .metadata()
        .is_ok_and(|metadata| metadata.len() >= RESIDENT_LOG_MAX_BYTES)
    {
        for index in (1..=RESIDENT_LOG_ROTATIONS).rev() {
            let source = if index == 1 {
                path.clone()
            } else {
                directory.join(format!("supervisor.log.{}", index - 1))
            };
            let destination = directory.join(format!("supervisor.log.{index}"));
            if source.exists() {
                let _ = std::fs::rename(source, destination);
            }
        }
    }
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("open resident log {}", path.display()))
}

fn write_sync_frame<T: serde::Serialize>(
    writer: &mut impl Write,
    value: &T,
    maximum: usize,
) -> Result<()> {
    let bytes = serde_json::to_vec(value)?;
    anyhow::ensure!(
        bytes.len() <= maximum,
        "bootstrap frame exceeds {maximum} bytes"
    );
    writer.write_all(&(bytes.len() as u32).to_be_bytes())?;
    writer.write_all(&bytes)?;
    writer.flush()?;
    Ok(())
}

fn read_sync_frame<T: serde::de::DeserializeOwned>(
    reader: &mut impl Read,
    maximum: usize,
) -> Result<T> {
    let mut length = [0_u8; 4];
    reader.read_exact(&mut length)?;
    let length = u32::from_be_bytes(length) as usize;
    anyhow::ensure!(
        length <= maximum,
        "bootstrap frame declares {length} bytes; limit is {maximum}"
    );
    let mut bytes = vec![0; length];
    reader.read_exact(&mut bytes)?;
    Ok(serde_json::from_slice(&bytes)?)
}

impl ResidentSocket {
    /// Bind while the caller holds the project lock. Only this authority may
    /// remove a stale pathname.
    pub fn bind(
        project: &Path,
        board: BoardBackend,
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
        let state = ServerState {
            board,
            actions,
            supervisor_token,
            sessions: Arc::default(),
        };
        let accept_task = tokio::spawn(accept_loop(listener, state, stop.clone()));
        Ok(Self {
            path,
            stop,
            accept_task: Some(accept_task),
        })
    }

    /// Give existing writers one final bounded scheduling opportunity, then
    /// stop accepting and remove the pathname while lock authority is held.
    pub async fn close(mut self) {
        tokio::time::sleep(FRAME_WRITE_TIMEOUT.min(Duration::from_millis(250))).await;
        self.stop.cancel();
        if let Some(accept_task) = self.accept_task.take() {
            let _ = accept_task.await;
        }
        let _ = std::fs::remove_file(&self.path);
    }
}

impl Drop for ResidentSocket {
    fn drop(&mut self) {
        // ResidentSocket is created only after project-lock acquisition and
        // is dropped before that lock guard, including error unwinds.
        self.stop.cancel();
        let _ = std::fs::remove_file(&self.path);
    }
}

pub fn supervisor_socket_path(project: &Path) -> Result<PathBuf> {
    let path = crate::runtime_paths::RuntimePaths::for_root(project).supervisor_socket();
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

async fn accept_loop(listener: UnixListener, state: ServerState, stop: CancellationToken) {
    loop {
        tokio::select! {
            _ = stop.cancelled() => return,
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => {
                    let state = state.clone();
                    tokio::spawn(async move {
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
    anyhow::ensure!(
        request.protocol_version == SUPERVISOR_PROTOCOL_VERSION,
        "unsupported supervisor protocol version {}",
        request.protocol_version
    );
    let generation = state.board.supervisor_snapshot().supervisor_generation;
    match request.role {
        ConnectionRole::Snapshots => {
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
            serve_snapshots(stream, state.board).await
        }
        ConnectionRole::Commands => {
            let session = issue_or_resume_session(&state.sessions, request.resume_command_session);
            write_frame(
                &mut stream,
                &HandshakeReply {
                    protocol_version: SUPERVISOR_PROTOCOL_VERSION,
                    supervisor_generation: generation,
                    command_session: Some(session),
                },
                MAX_HANDSHAKE_FRAME_BYTES,
                FRAME_WRITE_TIMEOUT,
            )
            .await?;
            serve_commands(stream, state, session).await
        }
    }
}

async fn serve_snapshots(mut stream: UnixStream, board: BoardBackend) -> Result<()> {
    let mut snapshots = board.subscribe();
    loop {
        let snapshot = snapshots.borrow_and_update().clone();
        phoxal_cli_core::session::protocol::validate_snapshot_bounds(&snapshot)
            .context("supervisor produced an out-of-bounds snapshot")?;
        write_frame(
            &mut stream,
            &snapshot,
            MAX_SNAPSHOT_FRAME_BYTES,
            FRAME_WRITE_TIMEOUT,
        )
        .await?;
        if matches!(
            snapshot.lifecycle,
            phoxal_cli_core::session::ProjectLifecycle::Stopped
                | phoxal_cli_core::session::ProjectLifecycle::Failed
        ) {
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

fn process_command(
    state: &ServerState,
    connection_session: CommandSessionId,
    request: CommandRequest,
) -> CommandReply {
    let current = state.board.supervisor_snapshot();
    if request.supervisor_generation != current.supervisor_generation {
        return CommandReply::rejected(CommandError::StaleSupervisorGeneration);
    }
    if request.key.session != connection_session {
        return CommandReply::rejected(CommandError::InvalidSession);
    }

    let mut sessions = state
        .sessions
        .lock()
        .expect("command sessions mutex poisoned");
    expire_sessions(&mut sessions);
    sessions.pending_restarts.retain(|key, incarnation| {
        current
            .processes
            .get(key)
            .and_then(|entry| entry.status.incarnation)
            == Some(*incarnation)
    });
    {
        let Some(session) = sessions.active.get_mut(&connection_session) else {
            return CommandReply::rejected(CommandError::InvalidSession);
        };
        session.last_used = Instant::now();
        if request.key.sequence <= session.highest_processed {
            return session
                .recent_replies
                .get(&request.key.sequence)
                .cloned()
                .unwrap_or_else(|| CommandReply::rejected(CommandError::AlreadyProcessed));
        }
        if request.key.sequence != session.highest_processed.saturating_add(1) {
            return CommandReply::rejected(CommandError::OutOfOrder);
        }
    }

    let reply = match request.action {
        CommandAction::Restart {
            process,
            expected_incarnation,
        } => match current.processes.get(&process) {
            None => CommandReply::rejected(CommandError::UnknownProcess),
            Some(entry) if entry.status.incarnation != Some(expected_incarnation) => {
                CommandReply::rejected(CommandError::SupersededIncarnation)
            }
            Some(_) if sessions.pending_restarts.get(&process) == Some(&expected_incarnation) => {
                CommandReply::rejected(CommandError::AlreadyProcessed)
            }
            Some(_) => match state.actions.try_send(SupervisorAction::Restart {
                key: process.clone(),
            }) {
                Ok(()) => {
                    sessions
                        .pending_restarts
                        .insert(process, expected_incarnation);
                    CommandReply::accepted()
                }
                Err(_) => CommandReply::rejected(CommandError::SupervisorUnavailable),
            },
        },
        CommandAction::Shutdown => {
            state.supervisor_token.cancel();
            CommandReply::accepted()
        }
    };
    let session = sessions
        .active
        .get_mut(&connection_session)
        .expect("validated command session disappeared while mutex held");
    session.highest_processed = request.key.sequence;
    session
        .recent_replies
        .insert(request.key.sequence, reply.clone());
    while session.recent_replies.len() > MAX_RECENT_COMMAND_REPLIES {
        let Some(oldest) = session.recent_replies.keys().next().copied() else {
            break;
        };
        session.recent_replies.remove(&oldest);
    }
    reply
}

fn issue_or_resume_session(
    sessions: &Arc<Mutex<CommandSessions>>,
    requested: Option<CommandSessionId>,
) -> CommandSessionId {
    let mut sessions = sessions.lock().expect("command sessions mutex poisoned");
    expire_sessions(&mut sessions);
    if let Some(requested) = requested
        && let Some(session) = sessions.active.get_mut(&requested)
    {
        session.last_used = Instant::now();
        return requested;
    }
    while sessions.active.len() >= MAX_COMMAND_SESSIONS {
        let Some(oldest) = sessions
            .active
            .iter()
            .min_by_key(|(_, state)| state.last_used)
            .map(|(id, _)| *id)
        else {
            break;
        };
        sessions.active.remove(&oldest);
    }
    loop {
        let mut bytes = [0_u8; 16];
        getrandom::fill(&mut bytes).expect("operating-system CSPRNG unavailable");
        let id = CommandSessionId(bytes);
        if let std::collections::hash_map::Entry::Vacant(entry) = sessions.active.entry(id) {
            entry.insert(CommandSessionState {
                highest_processed: 0,
                recent_replies: BTreeMap::new(),
                last_used: Instant::now(),
            });
            return id;
        }
    }
}

fn expire_sessions(sessions: &mut CommandSessions) {
    sessions
        .active
        .retain(|_, session| session.last_used.elapsed() < COMMAND_SESSION_TTL);
}

#[cfg(test)]
mod tests {
    use super::*;
    use phoxal_cli_core::session::{
        ParticipantKind, ParticipantStatus, ProcessKey, StartupRequirement,
    };

    #[test]
    fn socket_path_is_project_local_and_rejects_overlong_paths() {
        let project = Path::new("/tmp/phoxal-project");
        assert_eq!(
            supervisor_socket_path(project).unwrap(),
            project.join(".phoxal/supervisor.sock")
        );
        let long = PathBuf::from(format!("/tmp/{}", "x".repeat(256)));
        assert!(
            supervisor_socket_path(&long)
                .unwrap_err()
                .to_string()
                .contains("shorter")
        );
    }

    #[test]
    fn command_watermark_replays_and_fences_incarnations() {
        let board = BoardBackend::new();
        let key = ProcessKey::project("worker");
        board.upsert_process(
            key.clone(),
            ParticipantStatus::new(
                "worker",
                ParticipantKind::Service,
                crate::supervisor::ParticipantState::Ready,
            ),
            StartupRequirement::Required,
        );
        board.set_incarnation(&key, 7);
        let (actions, mut action_rx) = mpsc::channel(4);
        let token = CancellationToken::new();
        let state = ServerState {
            board: board.clone(),
            actions,
            supervisor_token: token,
            sessions: Arc::default(),
        };
        let session = issue_or_resume_session(&state.sessions, None);
        let request = |sequence, incarnation| CommandRequest {
            supervisor_generation: board.supervisor_snapshot().supervisor_generation,
            key: phoxal_cli_core::session::CommandKey { session, sequence },
            action: CommandAction::Restart {
                process: key.clone(),
                expected_incarnation: incarnation,
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
            Some(CommandError::SupersededIncarnation)
        );
    }

    #[test]
    fn pending_restart_is_deduplicated_across_fresh_sessions_until_incarnation_advances() {
        let board = BoardBackend::new();
        let key = ProcessKey::project("worker");
        board.upsert_process(
            key.clone(),
            ParticipantStatus::new(
                "worker",
                ParticipantKind::Service,
                crate::supervisor::ParticipantState::Ready,
            ),
            StartupRequirement::Required,
        );
        board.set_incarnation(&key, 7);
        let (actions, mut action_rx) = mpsc::channel(4);
        let state = ServerState {
            board: board.clone(),
            actions,
            supervisor_token: CancellationToken::new(),
            sessions: Arc::default(),
        };
        let request = |session, incarnation| CommandRequest {
            supervisor_generation: board.supervisor_snapshot().supervisor_generation,
            key: phoxal_cli_core::session::CommandKey {
                session,
                sequence: 1,
            },
            action: CommandAction::Restart {
                process: key.clone(),
                expected_incarnation: incarnation,
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

        board.set_incarnation(&key, 8);
        let next_session = issue_or_resume_session(&state.sessions, None);
        assert!(process_command(&state, next_session, request(next_session, 8)).accepted);
        assert!(matches!(
            action_rx.try_recv(),
            Ok(SupervisorAction::Restart { key: queued }) if queued == key
        ));
    }

    #[test]
    fn evicted_replies_and_expired_sessions_never_execute_again() {
        let board = BoardBackend::new();
        let (actions, _action_rx) = mpsc::channel(1);
        let state = ServerState {
            board: board.clone(),
            actions,
            supervisor_token: CancellationToken::new(),
            sessions: Arc::default(),
        };
        let session = issue_or_resume_session(&state.sessions, None);
        let generation = board.supervisor_snapshot().supervisor_generation;
        for sequence in 1..=(MAX_RECENT_COMMAND_REPLIES as u64 + 1) {
            let reply = process_command(
                &state,
                session,
                CommandRequest {
                    supervisor_generation: generation,
                    key: phoxal_cli_core::session::CommandKey { session, sequence },
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
                key: phoxal_cli_core::session::CommandKey {
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
                key: phoxal_cli_core::session::CommandKey {
                    session,
                    sequence: MAX_RECENT_COMMAND_REPLIES as u64 + 2,
                },
                action: CommandAction::Shutdown,
            },
        );
        assert_eq!(expired.error, Some(CommandError::InvalidSession));
    }

    #[tokio::test]
    async fn clients_receive_current_then_converge_on_latest_snapshot() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let board = BoardBackend::new();
        board.configure(temp.path().display().to_string(), "v0.test", "run");
        let (actions, _action_rx) = mpsc::channel(4);
        let token = CancellationToken::new();
        let socket = ResidentSocket::bind(temp.path(), board.clone(), actions, token)?;
        let first =
            phoxal_cli_client::SupervisorClient::connect(supervisor_socket_path(temp.path())?)
                .await?;
        let second =
            phoxal_cli_client::SupervisorClient::connect(supervisor_socket_path(temp.path())?)
                .await?;
        assert_eq!(
            first.snapshots().current().lifecycle,
            phoxal_cli_core::session::ProjectLifecycle::Starting
        );
        assert_eq!(
            first.snapshots().current().supervisor_generation,
            second.snapshots().current().supervisor_generation
        );

        let mut updates = first.snapshots().subscribe();
        let before = updates.borrow().revision;
        board.set_lifecycle(phoxal_cli_core::session::ProjectLifecycle::Ready);
        updates.changed().await?;
        assert!(updates.borrow().revision > before);
        assert_eq!(
            updates.borrow().lifecycle,
            phoxal_cli_core::session::ProjectLifecycle::Ready
        );
        board.set_lifecycle(phoxal_cli_core::session::ProjectLifecycle::Stopped);
        updates.changed().await?;
        socket.close().await;
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn idle_command_connection_outlives_the_frame_deadline() -> Result<()> {
        let board = BoardBackend::new();
        let (actions, _action_rx) = mpsc::channel(4);
        let state = ServerState {
            board: board.clone(),
            actions,
            supervisor_token: CancellationToken::new(),
            sessions: Arc::default(),
        };
        let session = issue_or_resume_session(&state.sessions, None);
        let generation = board.supervisor_snapshot().supervisor_generation;
        let (mut client_stream, server_stream) = UnixStream::pair()?;
        let server = tokio::spawn(serve_commands(server_stream, state, session));

        tokio::time::advance(FRAME_READ_TIMEOUT + Duration::from_secs(1)).await;
        write_frame(
            &mut client_stream,
            &CommandRequest {
                supervisor_generation: generation,
                key: phoxal_cli_core::session::CommandKey {
                    session,
                    sequence: 1,
                },
                action: CommandAction::Shutdown,
            },
            MAX_COMMAND_FRAME_BYTES,
            FRAME_WRITE_TIMEOUT,
        )
        .await?;
        let reply: CommandReply = read_frame(
            &mut client_stream,
            MAX_COMMAND_FRAME_BYTES,
            FRAME_READ_TIMEOUT,
        )
        .await?;
        assert!(reply.accepted);
        server.abort();
        Ok(())
    }

    #[tokio::test]
    async fn dropping_socket_cancels_accept_and_removes_path() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let path = supervisor_socket_path(temp.path())?;
        let board = BoardBackend::new();
        let (actions, _action_rx) = mpsc::channel(1);
        let socket = ResidentSocket::bind(temp.path(), board, actions, CancellationToken::new())?;
        assert!(path.exists());
        drop(socket);
        assert!(!path.exists());
        tokio::task::yield_now().await;
        Ok(())
    }
}
