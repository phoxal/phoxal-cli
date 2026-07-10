use std::collections::{BTreeMap, VecDeque};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use phoxal::bus::{Subscribe, Subscriber, Topic};
use phoxal::raw::{Bus, BusConfig};
use phoxal_api::y2026_1 as api;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::{MissedTickBehavior, timeout};

use crate::launch_plan::DEFAULT_ROUTER_CONNECT;

pub const SUPERVISOR_LOCK_FILE: &str = "supervisor.lock";
pub const SUPERVISOR_STATE_FILE: &str = "supervisor-state.json";
pub const SUPERVISOR_ACTIONS_FILE: &str = "supervisor-actions.jsonl";
pub const RESTART_SEC: Duration = Duration::from_secs(2);
pub const START_LIMIT_INTERVAL: Duration = Duration::from_secs(60);
pub const START_LIMIT_BURST: usize = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParticipantKind {
    SiteTool,
    OfficialArtifact,
    UserService,
    ComponentDriver,
}

impl ParticipantKind {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::SiteTool => "site-tool",
            Self::OfficialArtifact => "official",
            Self::UserService => "user-service",
            Self::ComponentDriver => "driver",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParticipantState {
    Starting,
    Ready,
    Degraded,
    Failed,
    Restarting,
    Released,
}

impl ParticipantState {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Ready => "ready",
            Self::Degraded => "degraded",
            Self::Failed => "failed",
            Self::Restarting => "restarting",
            Self::Released => "released",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ParticipantLaunchCommand {
    pub command_line: String,
    pub env: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ParticipantStatus {
    pub id: String,
    pub kind: ParticipantKind,
    pub state: ParticipantState,
    pub restart_count: u32,
    pub note: Option<String>,
    pub last_log_line: Option<String>,
    pub last_log_lines: Vec<String>,
    pub launch_command: Option<ParticipantLaunchCommand>,
}

impl ParticipantStatus {
    #[must_use]
    pub fn new(id: impl Into<String>, kind: ParticipantKind, state: ParticipantState) -> Self {
        Self {
            id: id.into(),
            kind,
            state,
            restart_count: 0,
            note: None,
            last_log_line: None,
            last_log_lines: Vec::new(),
            launch_command: None,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct BoardSnapshot {
    pub participants: BTreeMap<String, ParticipantStatus>,
}

impl BoardSnapshot {
    #[must_use]
    pub fn failed_participants(&self) -> Vec<String> {
        self.participants
            .values()
            .filter(|participant| participant.state == ParticipantState::Failed)
            .map(|participant| participant.id.clone())
            .collect()
    }

    #[must_use]
    pub fn has_running_state(&self) -> bool {
        self.participants.values().any(|participant| {
            matches!(
                participant.state,
                ParticipantState::Starting | ParticipantState::Ready | ParticipantState::Restarting
            )
        })
    }

    #[must_use]
    pub fn render(&self) -> String {
        let mut out = String::from(
            "participant                 kind          state       restarts  note  last log\n",
        );
        out.push_str(
            "--------------------------------------------------------------------------------\n",
        );
        for participant in self.participants.values() {
            let note = participant.note.as_deref().unwrap_or("-");
            let last = participant.last_log_line.as_deref().unwrap_or("-");
            out.push_str(&format!(
                "{:<27} {:<13} {:<11} {:>8}  {}  {}\n",
                trim_cell(&participant.id, 27),
                participant.kind.label(),
                participant.state.label(),
                participant.restart_count,
                trim_cell(note, 44),
                trim_cell(last, 72),
            ));
        }
        out
    }
}

fn trim_cell(value: &str, width: usize) -> String {
    let count = value.chars().count();
    if count <= width {
        return value.to_string();
    }
    if width <= 1 {
        return ".".to_string();
    }
    value.chars().take(width - 1).collect::<String>() + "."
}

#[derive(Debug, Clone, Default)]
pub struct BoardBackend {
    inner: Arc<Mutex<BoardSnapshot>>,
}

impl BoardBackend {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn upsert(&self, status: ParticipantStatus) {
        self.inner
            .lock()
            .expect("board mutex poisoned")
            .participants
            .insert(status.id.clone(), status);
    }

    pub fn set_state(&self, id: &str, state: ParticipantState, note: Option<String>) {
        let mut snapshot = self.inner.lock().expect("board mutex poisoned");
        if let Some(status) = snapshot.participants.get_mut(id) {
            status.state = state;
            if note.is_some() {
                status.note = note;
            }
        }
    }

    pub fn set_note(&self, id: &str, note: impl Into<String>) {
        let mut snapshot = self.inner.lock().expect("board mutex poisoned");
        let status = snapshot
            .participants
            .entry(id.to_string())
            .or_insert_with(|| {
                ParticipantStatus::new(
                    id,
                    ParticipantKind::OfficialArtifact,
                    ParticipantState::Ready,
                )
            });
        status.note = Some(note.into());
    }

    pub fn set_restart_count(&self, id: &str, count: u32) {
        let mut snapshot = self.inner.lock().expect("board mutex poisoned");
        if let Some(status) = snapshot.participants.get_mut(id) {
            status.restart_count = count;
        }
    }

    pub fn append_log(&self, id: &str, line: impl Into<String>) {
        let line = line.into();
        let mut snapshot = self.inner.lock().expect("board mutex poisoned");
        let status = snapshot
            .participants
            .entry(id.to_string())
            .or_insert_with(|| {
                ParticipantStatus::new(
                    id,
                    ParticipantKind::OfficialArtifact,
                    ParticipantState::Ready,
                )
            });
        status.last_log_line = Some(line.clone());
        status.last_log_lines.push(line);
        const MAX_LAST_LINES: usize = 8;
        if status.last_log_lines.len() > MAX_LAST_LINES {
            let drop_count = status.last_log_lines.len() - MAX_LAST_LINES;
            status.last_log_lines.drain(0..drop_count);
        }
    }

    pub fn set_launch_command(&self, id: &str, command: ParticipantLaunchCommand) {
        let mut snapshot = self.inner.lock().expect("board mutex poisoned");
        if let Some(status) = snapshot.participants.get_mut(id) {
            status.launch_command = Some(command);
        }
    }

    #[must_use]
    pub fn snapshot(&self) -> BoardSnapshot {
        self.inner.lock().expect("board mutex poisoned").clone()
    }

    pub fn write_snapshot(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let json = serde_json::to_string_pretty(&self.snapshot())
            .context("failed to serialize supervisor status")?;
        fs::write(path, json).with_context(|| format!("failed to write {}", path.display()))
    }
}

#[derive(Debug, Clone)]
pub struct ParticipantSpec {
    pub id: String,
    pub kind: ParticipantKind,
    pub executable: PathBuf,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub env: Vec<(String, String)>,
    pub shutdown_grace: Duration,
    pub note: Option<String>,
}

impl ParticipantSpec {
    #[must_use]
    pub fn command_line(&self) -> String {
        let mut parts = vec![self.executable.display().to_string()];
        parts.extend(self.args.clone());
        parts.join(" ")
    }

    #[must_use]
    pub fn launch_command(&self) -> ParticipantLaunchCommand {
        ParticipantLaunchCommand {
            command_line: render_manual_command_line(self),
            env: self.env.iter().cloned().collect(),
        }
    }
}

fn render_manual_command_line(spec: &ParticipantSpec) -> String {
    let env = spec.env.iter().cloned().collect::<BTreeMap<_, _>>();
    let mut parts = vec![shell_quote(&spec.executable.display().to_string())];
    parts.extend(spec.args.iter().map(|arg| shell_quote(arg)));
    for (env_key, flag) in crate::launch_env::ENV_TO_FLAG {
        if let Some(value) = env.get(*env_key) {
            parts.push((*flag).to_string());
            parts.push(shell_quote(value));
        }
    }
    parts.join(" ")
}

fn shell_quote(value: &str) -> String {
    if !value.is_empty()
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(byte, b'/' | b'.' | b'-' | b'_' | b':' | b',' | b'=' | b'@')
        })
    {
        return value.to_string();
    }
    let escaped = value.replace('\'', "'\\''");
    format!("'{escaped}'")
}

#[derive(Debug, Clone)]
pub struct RestartPolicy {
    pub restart_delay: Duration,
    pub start_limit_interval: Duration,
    pub start_limit_burst: usize,
}

impl Default for RestartPolicy {
    fn default() -> Self {
        Self {
            restart_delay: RESTART_SEC,
            start_limit_interval: START_LIMIT_INTERVAL,
            start_limit_burst: START_LIMIT_BURST,
        }
    }
}

#[derive(Debug)]
pub struct SupervisorOptions {
    pub restart_policy: RestartPolicy,
    pub state_file: Option<PathBuf>,
    pub action_file: Option<PathBuf>,
    pub action_rx: Option<mpsc::Receiver<SupervisorAction>>,
    pub render_board: bool,
}

impl Default for SupervisorOptions {
    fn default() -> Self {
        Self {
            restart_policy: RestartPolicy::default(),
            state_file: None,
            action_file: None,
            action_rx: None,
            render_board: true,
        }
    }
}

#[derive(Debug)]
pub enum SupervisorAction {
    Swap {
        id: String,
        spec: ParticipantSpec,
        note: String,
    },
    Release {
        id: String,
    },
    Resume {
        id: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum SupervisorActionRequest {
    Release { participant: String },
    Resume { participant: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SupervisorOutcome {
    pub clean_shutdown: bool,
    pub failed_participants: Vec<String>,
}

impl SupervisorOutcome {
    #[must_use]
    pub fn graph_healthy(&self) -> bool {
        self.failed_participants.is_empty()
    }
}

struct RunningParticipant {
    spec: ParticipantSpec,
    child: Option<Child>,
    stdout_task: Option<JoinHandle<()>>,
    stderr_task: Option<JoinHandle<()>>,
    failure_times: VecDeque<Instant>,
    restart_count: u32,
    restart_at: Option<Instant>,
    failed: bool,
    released: bool,
}

impl RunningParticipant {
    async fn spawn(spec: ParticipantSpec, board: &BoardBackend) -> Result<Self> {
        let mut running = Self {
            spec,
            child: None,
            stdout_task: None,
            stderr_task: None,
            failure_times: VecDeque::new(),
            restart_count: 0,
            restart_at: None,
            failed: false,
            released: false,
        };
        running.spawn_child(board).await?;
        Ok(running)
    }

    async fn spawn_child(&mut self, board: &BoardBackend) -> Result<()> {
        board.set_state(
            &self.spec.id,
            ParticipantState::Starting,
            self.spec.note.clone(),
        );
        let mut command = Command::new(&self.spec.executable);
        command.args(&self.spec.args);
        if let Some(cwd) = &self.spec.cwd {
            command.current_dir(cwd);
        }
        command
            .envs(self.spec.env.iter().map(|(key, value)| (key, value)))
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command
            .spawn()
            .with_context(|| format!("failed to spawn {}", self.spec.command_line()))?;
        let pid = child.id().unwrap_or_default();
        board.append_log(&self.spec.id, format!("supervisor: spawned pid {pid}"));
        board.set_launch_command(&self.spec.id, self.spec.launch_command());
        if let Some(stdout) = child.stdout.take() {
            self.stdout_task = Some(spawn_output_reader(
                board.clone(),
                self.spec.id.clone(),
                "stdout",
                stdout,
            ));
        }
        if let Some(stderr) = child.stderr.take() {
            self.stderr_task = Some(spawn_output_reader(
                board.clone(),
                self.spec.id.clone(),
                "stderr",
                stderr,
            ));
        }
        self.child = Some(child);
        board.set_state(
            &self.spec.id,
            ParticipantState::Ready,
            self.spec.note.clone(),
        );
        Ok(())
    }

    async fn poll(&mut self, board: &BoardBackend, policy: &RestartPolicy) -> Result<()> {
        if self.failed || self.released {
            return Ok(());
        }
        if let Some(restart_at) = self.restart_at {
            if Instant::now() < restart_at {
                return Ok(());
            }
            self.restart_at = None;
            self.spawn_child(board).await?;
            return Ok(());
        }
        let Some(child) = self.child.as_mut() else {
            return Ok(());
        };
        let Some(status) = child
            .try_wait()
            .with_context(|| format!("failed to poll {}", self.spec.id))?
        else {
            return Ok(());
        };

        self.child = None;
        join_reader(self.stdout_task.take()).await;
        join_reader(self.stderr_task.take()).await;

        if status.success() {
            self.failed = true;
            board.set_state(
                &self.spec.id,
                ParticipantState::Failed,
                Some(
                    "exited successfully; supervisor expected a long-running participant"
                        .to_string(),
                ),
            );
            return Ok(());
        }

        let now = Instant::now();
        self.failure_times
            .retain(|failure| now.duration_since(*failure) <= policy.start_limit_interval);
        self.failure_times.push_back(now);
        if self.failure_times.len() >= policy.start_limit_burst {
            self.failed = true;
            board.set_state(
                &self.spec.id,
                ParticipantState::Failed,
                Some(format!(
                    "StartLimitBurst exhausted after {} failures in {}s; last status {status}",
                    policy.start_limit_burst,
                    policy.start_limit_interval.as_secs()
                )),
            );
            return Ok(());
        }

        self.restart_count = self.restart_count.saturating_add(1);
        board.set_restart_count(&self.spec.id, self.restart_count);
        board.set_state(
            &self.spec.id,
            ParticipantState::Restarting,
            Some(format!(
                "exited with {status}; restarting in {}s",
                policy.restart_delay.as_secs_f32()
            )),
        );
        self.restart_at = Some(now + policy.restart_delay);
        Ok(())
    }

    fn is_active(&self) -> bool {
        !self.failed && !self.released && (self.child.is_some() || self.restart_at.is_some())
    }

    async fn stop_current(&mut self, board: &BoardBackend) -> Result<()> {
        if let Some(mut child) = self.child.take() {
            board.append_log(&self.spec.id, "supervisor: stopping");
            stop_child(&mut child, self.spec.shutdown_grace).await?;
        }
        join_reader(self.stdout_task.take()).await;
        join_reader(self.stderr_task.take()).await;
        Ok(())
    }

    async fn swap(
        &mut self,
        spec: ParticipantSpec,
        board: &BoardBackend,
        note: String,
    ) -> Result<()> {
        self.stop_current(board).await?;
        self.spec = spec;
        self.failed = false;
        self.released = false;
        self.restart_at = None;
        self.spawn_child(board).await?;
        board.set_state(&self.spec.id, ParticipantState::Ready, Some(note));
        Ok(())
    }

    async fn release(&mut self, board: &BoardBackend) -> Result<()> {
        if self.released {
            board.set_state(
                &self.spec.id,
                ParticipantState::Released,
                Some("released for manual run".to_string()),
            );
            return Ok(());
        }
        self.stop_current(board).await?;
        self.released = true;
        self.failed = false;
        self.restart_at = None;
        board.set_state(
            &self.spec.id,
            ParticipantState::Released,
            Some("released for manual run".to_string()),
        );
        Ok(())
    }

    async fn resume(&mut self, board: &BoardBackend) -> Result<()> {
        if !self.released {
            board.set_note(&self.spec.id, "already supervised");
            return Ok(());
        }
        self.released = false;
        self.failed = false;
        self.restart_at = None;
        self.spawn_child(board).await?;
        Ok(())
    }
}

fn spawn_output_reader<R>(
    board: BoardBackend,
    id: String,
    stream: &'static str,
    reader: R,
) -> JoinHandle<()>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut lines = BufReader::new(reader).lines();
        loop {
            match lines.next_line().await {
                Ok(Some(line)) => board.append_log(&id, format!("{stream}: {line}")),
                Ok(None) => break,
                Err(error) => {
                    board.append_log(&id, format!("supervisor: failed to read {stream}: {error}"));
                    break;
                }
            }
        }
    })
}

async fn join_reader(task: Option<JoinHandle<()>>) {
    if let Some(task) = task {
        let _ = timeout(Duration::from_millis(250), task).await;
    }
}

pub async fn supervise_until_shutdown(
    specs: Vec<ParticipantSpec>,
    board: BoardBackend,
    mut options: SupervisorOptions,
) -> Result<SupervisorOutcome> {
    let mut running = Vec::new();
    for spec in specs {
        let id = spec.id.clone();
        match RunningParticipant::spawn(spec, &board).await {
            Ok(participant) => running.push(participant),
            Err(error) => {
                board.set_state(
                    &id,
                    ParticipantState::Failed,
                    Some(format!("spawn failed: {error:#}")),
                );
            }
        }
    }

    let mut ticker = tokio::time::interval(Duration::from_millis(500));
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut clean_shutdown = false;
    let mut board_ticks = 0_u64;
    let mut action_rx = options.action_rx.take();
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                clean_shutdown = true;
                break;
            }
            action = recv_action(&mut action_rx) => {
                if let Some(action) = action {
                    handle_action(&mut running, &board, action).await?;
                }
            }
            _ = ticker.tick() => {
                if let Some(action_file) = &options.action_file {
                    for action in drain_action_file(action_file)? {
                        handle_action(&mut running, &board, action).await?;
                    }
                }
                for participant in &mut running {
                    participant.poll(&board, &options.restart_policy).await?;
                }
                board_ticks = board_ticks.saturating_add(1);
                if options.render_board && board_ticks % 2 == 1 {
                    eprintln!("{}", board.snapshot().render());
                }
                if let Some(state_file) = &options.state_file {
                    board.write_snapshot(state_file)?;
                }
                if !running.iter().any(RunningParticipant::is_active) {
                    break;
                }
            }
        }
    }

    shutdown_all(&mut running, &board).await;
    if let Some(state_file) = &options.state_file {
        board.write_snapshot(state_file)?;
    }
    Ok(SupervisorOutcome {
        clean_shutdown,
        failed_participants: board.snapshot().failed_participants(),
    })
}

async fn recv_action(
    action_rx: &mut Option<mpsc::Receiver<SupervisorAction>>,
) -> Option<SupervisorAction> {
    match action_rx {
        Some(action_rx) => action_rx.recv().await,
        None => std::future::pending().await,
    }
}

async fn handle_action(
    running: &mut [RunningParticipant],
    board: &BoardBackend,
    action: SupervisorAction,
) -> Result<()> {
    match action {
        SupervisorAction::Swap { id, spec, note } => {
            let Some(participant) = running
                .iter_mut()
                .find(|participant| participant.spec.id == id)
            else {
                board.append_log(&id, "supervisor: swap requested for unknown participant");
                return Ok(());
            };
            participant.swap(spec, board, note).await
        }
        SupervisorAction::Release { id } => {
            let Some(participant) = running
                .iter_mut()
                .find(|participant| participant.spec.id == id)
            else {
                board.append_log(&id, "supervisor: release requested for unknown participant");
                return Ok(());
            };
            participant.release(board).await
        }
        SupervisorAction::Resume { id } => {
            let Some(participant) = running
                .iter_mut()
                .find(|participant| participant.spec.id == id)
            else {
                board.append_log(&id, "supervisor: resume requested for unknown participant");
                return Ok(());
            };
            participant.resume(board).await
        }
    }
}

fn drain_action_file(path: &Path) -> Result<Vec<SupervisorAction>> {
    if !path.is_file() {
        return Ok(Vec::new());
    }
    let processing = path.with_extension("processing");
    match fs::rename(path, &processing) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(error).with_context(|| {
                format!("failed to drain supervisor action file {}", path.display())
            });
        }
    }
    let contents = fs::read_to_string(&processing).with_context(|| {
        format!(
            "failed to read supervisor action file {}",
            processing.display()
        )
    })?;
    let _ = fs::remove_file(&processing);
    contents
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let request = serde_json::from_str::<SupervisorActionRequest>(line)
                .context("supervisor action JSON was invalid")?;
            Ok(match request {
                SupervisorActionRequest::Release { participant } => {
                    SupervisorAction::Release { id: participant }
                }
                SupervisorActionRequest::Resume { participant } => {
                    SupervisorAction::Resume { id: participant }
                }
            })
        })
        .collect()
}

async fn shutdown_all(running: &mut [RunningParticipant], board: &BoardBackend) {
    for participant in running.iter_mut().rev() {
        if let Some(mut child) = participant.child.take() {
            board.append_log(&participant.spec.id, "supervisor: stopping");
            if let Err(error) = stop_child(&mut child, participant.spec.shutdown_grace).await {
                board.set_state(
                    &participant.spec.id,
                    ParticipantState::Failed,
                    Some(format!("failed to stop: {error:#}")),
                );
            }
        }
        join_reader(participant.stdout_task.take()).await;
        join_reader(participant.stderr_task.take()).await;
    }
}

pub async fn stop_child(child: &mut Child, budget: Duration) -> Result<()> {
    if let Some(pid) = child.id() {
        send_terminate(pid).await;
    }
    match timeout(budget, child.wait()).await {
        Ok(status) => {
            status.context("failed to wait for child")?;
            Ok(())
        }
        Err(_) => {
            child.start_kill().context("failed to kill child")?;
            child.wait().await.context("failed to wait after kill")?;
            Ok(())
        }
    }
}

async fn send_terminate(pid: u32) {
    #[cfg(unix)]
    {
        let _ = Command::new("kill")
            .arg("-TERM")
            .arg(pid.to_string())
            .status()
            .await;
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
    }
}

#[derive(Debug)]
pub struct SupervisorLock {
    path: PathBuf,
    owned: bool,
}

impl SupervisorLock {
    pub fn acquire(run_dir: &Path) -> Result<Self> {
        fs::create_dir_all(run_dir)
            .with_context(|| format!("failed to create {}", run_dir.display()))?;
        Self::acquire_path(&run_dir.join(SUPERVISOR_LOCK_FILE))
    }

    pub fn acquire_path(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let path = path.to_path_buf();
        match try_create_lock(&path) {
            Ok(()) => Ok(Self { path, owned: true }),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let existing_pid = fs::read_to_string(&path)
                    .ok()
                    .and_then(|contents| contents.trim().parse::<u32>().ok());
                if existing_pid.is_some_and(pid_alive) {
                    bail!(
                        "another phoxal-cli supervisor session is already active on this host (lock: {})",
                        path.display()
                    );
                }
                let _ = fs::remove_file(&path);
                try_create_lock(&path).with_context(|| {
                    format!("failed to replace stale supervisor lock {}", path.display())
                })?;
                Ok(Self { path, owned: true })
            }
            Err(error) => Err(error)
                .with_context(|| format!("failed to create supervisor lock {}", path.display())),
        }
    }
}

impl Drop for SupervisorLock {
    fn drop(&mut self) {
        if self.owned {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn try_create_lock(path: &Path) -> std::io::Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    writeln!(file, "{}", std::process::id())
}

fn pid_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        std::process::Command::new("kill")
            .arg("-0")
            .arg(pid.to_string())
            .status()
            .is_ok_and(|status| status.success())
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        false
    }
}

pub fn local_router_reachable(endpoint: &str) -> bool {
    endpoint_reachable(endpoint, Duration::from_millis(500))
}

pub fn endpoint_reachable(endpoint: &str, timeout: Duration) -> bool {
    let Some(address) = endpoint.strip_prefix("tcp/") else {
        return false;
    };
    let Ok(mut addresses) = address.to_socket_addrs() else {
        return false;
    };
    let Some(address) = addresses.next() else {
        return false;
    };
    TcpStream::connect_timeout(&address, timeout).is_ok()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouterOwnership {
    External,
    Managed,
}

pub fn router_ownership(endpoint_reachable: bool) -> RouterOwnership {
    if endpoint_reachable {
        RouterOwnership::External
    } else {
        RouterOwnership::Managed
    }
}

#[must_use]
pub fn teardown_order(specs: &[ParticipantSpec]) -> Vec<String> {
    specs.iter().rev().map(|spec| spec.id.clone()).collect()
}

pub fn supervisor_state_path() -> Result<PathBuf> {
    Ok(crate::host_paths::run_dir()?.join(SUPERVISOR_STATE_FILE))
}

pub fn supervisor_actions_path() -> Result<PathBuf> {
    Ok(crate::host_paths::run_dir()?.join(SUPERVISOR_ACTIONS_FILE))
}

pub fn read_supervisor_state(path: &Path) -> Result<BoardSnapshot> {
    let text =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    serde_json::from_str(&text)
        .with_context(|| format!("{} is not valid supervisor status JSON", path.display()))
}

pub fn request_supervisor_action(request: SupervisorActionRequest) -> Result<()> {
    let path = supervisor_actions_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("failed to open supervisor action file {}", path.display()))?;
    serde_json::to_writer(&mut file, &request)
        .with_context(|| format!("failed to encode supervisor action {}", path.display()))?;
    writeln!(file).with_context(|| format!("failed to write {}", path.display()))
}

pub fn start_bus_log_subscriber(
    namespace: String,
    robot_id: String,
    connect: String,
    board: BoardBackend,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            match bus_log_subscriber_loop(
                namespace.clone(),
                robot_id.clone(),
                connect.clone(),
                board.clone(),
            )
            .await
            {
                Ok(()) => break,
                Err(error) => {
                    tracing::debug!("bus log subscriber waiting for router: {error:#}");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
        }
    })
}

async fn bus_log_subscriber_loop(
    namespace: String,
    robot_id: String,
    connect: String,
    board: BoardBackend,
) -> Result<()> {
    let bus = Bus::open(BusConfig {
        namespace,
        robot_id,
        participant: "phoxal-cli-supervisor".to_string(),
        incarnation: 0,
        connect_endpoints: vec![connect],
    })
    .await
    .map_err(|error| anyhow!("failed to open bus log subscription: {error}"))?;
    let topic = Topic::<Subscribe<api::logs::Event>>::new_owned("logs/*".to_string());
    let subscriber = Subscriber::<api::logs::Event>::new(&bus, &topic, 128).await?;
    loop {
        let received = subscriber.recv().await?;
        let id = received.metadata.source.participant;
        board.append_log(&id, render_log_event(&received.body));
    }
}

#[must_use]
pub fn render_log_event(event: &api::logs::Event) -> String {
    let mut message = format!("{:?}: {}", event.level, event.message);
    if event.dropped > 0 {
        message.push_str(&format!(" (dropped {})", event.dropped));
    }
    message
}

#[must_use]
pub fn default_connect_endpoint() -> String {
    DEFAULT_ROUTER_CONNECT.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(id: &str) -> ParticipantSpec {
        ParticipantSpec {
            id: id.to_string(),
            kind: ParticipantKind::UserService,
            executable: PathBuf::from("/bin/echo"),
            args: Vec::new(),
            cwd: None,
            env: Vec::new(),
            shutdown_grace: Duration::from_millis(10),
            note: None,
        }
    }

    fn sleep_spec(id: &str) -> ParticipantSpec {
        ParticipantSpec {
            id: id.to_string(),
            kind: ParticipantKind::UserService,
            executable: PathBuf::from("/bin/sh"),
            args: vec!["-c".to_string(), "sleep 30".to_string()],
            cwd: None,
            env: Vec::new(),
            shutdown_grace: Duration::from_millis(50),
            note: None,
        }
    }

    #[test]
    fn router_ownership_distinguishes_external_from_managed() {
        assert_eq!(router_ownership(true), RouterOwnership::External);
        assert_eq!(router_ownership(false), RouterOwnership::Managed);
    }

    #[test]
    fn teardown_order_is_reverse_spawn_order_so_router_is_last() {
        let specs = vec![spec("tool-router"), spec("tool-joypad"), spec("mission")];
        assert_eq!(
            teardown_order(&specs),
            vec!["mission", "tool-joypad", "tool-router"]
        );
    }

    #[test]
    fn launch_command_prints_contract_flags_and_env() {
        let mut spec = spec("mission");
        spec.executable = PathBuf::from("/tmp/phoxal mission");
        spec.env = vec![
            (
                phoxal::participant::launch::env::PARTICIPANT_ID.to_string(),
                "mission".to_string(),
            ),
            (
                phoxal::participant::launch::env::ROBOT_ID.to_string(),
                "robot".to_string(),
            ),
            (
                phoxal::participant::launch::env::CONNECT.to_string(),
                "tcp/localhost:7447".to_string(),
            ),
        ];

        let launch = spec.launch_command();
        assert!(
            launch
                .command_line
                .contains("'/tmp/phoxal mission' --participant-id mission"),
            "{}",
            launch.command_line
        );
        assert!(
            launch.command_line.contains("--connect tcp/localhost:7447"),
            "{}",
            launch.command_line
        );
        assert_eq!(
            launch
                .env
                .get(phoxal::participant::launch::env::PARTICIPANT_ID)
                .map(String::as_str),
            Some("mission")
        );
    }

    #[tokio::test]
    async fn release_and_resume_stop_and_respawn_the_managed_child() -> Result<()> {
        let board = BoardBackend::new();
        board.upsert(ParticipantStatus::new(
            "mission",
            ParticipantKind::UserService,
            ParticipantState::Starting,
        ));
        let mut participant = RunningParticipant::spawn(sleep_spec("mission"), &board).await?;

        participant.release(&board).await?;
        let released = board.snapshot();
        let status = released.participants.get("mission").expect("mission");
        assert_eq!(status.state, ParticipantState::Released);
        assert!(!participant.is_active());

        participant.resume(&board).await?;
        let resumed = board.snapshot();
        let status = resumed.participants.get("mission").expect("mission");
        assert_eq!(status.state, ParticipantState::Ready);
        assert!(participant.is_active());

        participant.stop_current(&board).await?;
        Ok(())
    }

    #[tokio::test]
    async fn watch_swap_does_not_consume_restart_budget() -> Result<()> {
        let board = BoardBackend::new();
        board.upsert(ParticipantStatus::new(
            "mission",
            ParticipantKind::UserService,
            ParticipantState::Starting,
        ));
        let mut participant = RunningParticipant::spawn(sleep_spec("mission"), &board).await?;
        participant.restart_count = 4;
        participant
            .failure_times
            .push_back(Instant::now() - Duration::from_secs(1));

        participant
            .swap(
                sleep_spec("mission"),
                &board,
                "ok 0.1s, restarted".to_string(),
            )
            .await?;

        assert_eq!(participant.restart_count, 4);
        assert_eq!(participant.failure_times.len(), 1);
        let snapshot = board.snapshot();
        let status = snapshot.participants.get("mission").expect("mission");
        assert_eq!(status.state, ParticipantState::Ready);
        assert_eq!(status.note.as_deref(), Some("ok 0.1s, restarted"));

        participant.stop_current(&board).await?;
        Ok(())
    }

    #[tokio::test]
    async fn restart_burst_exhaustion_marks_failed() -> Result<()> {
        let board = BoardBackend::new();
        board.upsert(ParticipantStatus::new(
            "flap",
            ParticipantKind::UserService,
            ParticipantState::Starting,
        ));
        let specs = vec![ParticipantSpec {
            id: "flap".to_string(),
            kind: ParticipantKind::UserService,
            executable: PathBuf::from("/bin/sh"),
            args: vec!["-c".to_string(), "echo fail; exit 7".to_string()],
            cwd: None,
            env: Vec::new(),
            shutdown_grace: Duration::from_millis(10),
            note: None,
        }];
        let outcome = supervise_until_shutdown(
            specs,
            board.clone(),
            SupervisorOptions {
                restart_policy: RestartPolicy {
                    restart_delay: Duration::from_millis(1),
                    start_limit_interval: Duration::from_secs(60),
                    start_limit_burst: 3,
                },
                state_file: None,
                action_file: None,
                action_rx: None,
                render_board: false,
            },
        )
        .await?;

        assert_eq!(outcome.failed_participants, vec!["flap"]);
        let snapshot = board.snapshot();
        let status = snapshot.participants.get("flap").expect("flap status");
        assert_eq!(status.state, ParticipantState::Failed);
        assert!(
            status
                .note
                .as_deref()
                .is_some_and(|note| note.contains("StartLimitBurst")),
            "{status:?}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn local_log_capture_works_without_bus() -> Result<()> {
        let board = BoardBackend::new();
        board.upsert(ParticipantStatus::new(
            "logger",
            ParticipantKind::UserService,
            ParticipantState::Starting,
        ));
        let specs = vec![ParticipantSpec {
            id: "logger".to_string(),
            kind: ParticipantKind::UserService,
            executable: PathBuf::from("/bin/sh"),
            args: vec!["-c".to_string(), "echo local-line; exit 2".to_string()],
            cwd: None,
            env: Vec::new(),
            shutdown_grace: Duration::from_millis(10),
            note: None,
        }];
        let _ = supervise_until_shutdown(
            specs,
            board.clone(),
            SupervisorOptions {
                restart_policy: RestartPolicy {
                    restart_delay: Duration::from_millis(1),
                    start_limit_interval: Duration::from_secs(1),
                    start_limit_burst: 1,
                },
                state_file: None,
                action_file: None,
                action_rx: None,
                render_board: false,
            },
        )
        .await?;

        let snapshot = board.snapshot();
        let status = snapshot.participants.get("logger").expect("logger status");
        assert!(
            status
                .last_log_lines
                .iter()
                .any(|line| line.contains("stdout: local-line")),
            "{status:?}"
        );
        Ok(())
    }
}
