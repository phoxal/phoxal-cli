use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use phoxal::bus::{Subscribe, Subscriber, Topic};
use phoxal::raw::{Bus, BusConfig};
use phoxal_api::v1 as api;
// The simulation clock lives in preview `v2`; the presence and logs contracts
// this module also consumes use current production `v1`.
use phoxal_api::v2 as preview_api;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::{MissedTickBehavior, timeout};

use crate::launch_plan::DEFAULT_ROUTER_CONNECT;
pub use crate::participant_kind::ParticipantKind;
use crate::session::output::WaitBudget;

pub const SUPERVISOR_LOCK_FILE: &str = "supervisor.lock";
pub const RESTART_SEC: Duration = Duration::from_secs(2);
pub const START_LIMIT_INTERVAL: Duration = Duration::from_secs(60);
pub const START_LIMIT_BURST: usize = 5;

/// How long a bus participant may go silent (no `presence/heartbeat`) after
/// having been observed at least once before the supervisor marks it `Failed`.
/// Comfortably above the runner's 1 Hz heartbeat cadence
/// (`phoxal::participant::heartbeat::HEARTBEAT_INTERVAL`) and the presence
/// service's own 3 s stale threshold, so ordinary scheduling jitter never
/// trips it; a genuinely dead/hung participant still gets caught within one
/// supervisor board render cycle of the deadline passing.
pub const HEARTBEAT_STALE_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParticipantState {
    Starting,
    Ready,
    Degraded,
    Failed,
    Restarting,
    Stopped,
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
            Self::Stopped => "stopped",
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
    /// Whether this participant runs from a locally resolved directory
    /// (user-authored source, or a local path-pin override) rather than a
    /// fetched catalog artifact. Orthogonal to `kind` - see
    /// `crate::participant_kind` module docs. Defaults to `false`
    /// (catalog) via [`Self::new`]; set explicitly with [`Self::with_local`].
    #[serde(default)]
    pub local: bool,
    pub state: ParticipantState,
    pub restart_count: u32,
    pub note: Option<String>,
    pub last_log_line: Option<String>,
    pub last_log_lines: Vec<String>,
    pub launch_command: Option<ParticipantLaunchCommand>,
    /// Live child-process details for the interactive session only. These are
    /// intentionally excluded from the persisted/JSON-stable board shape.
    #[serde(skip)]
    pub pid: Option<u32>,
    #[serde(skip)]
    pub artifact_size_bytes: Option<u64>,
}

impl ParticipantStatus {
    #[must_use]
    pub fn new(id: impl Into<String>, kind: ParticipantKind, state: ParticipantState) -> Self {
        Self {
            id: id.into(),
            kind,
            local: false,
            state,
            restart_count: 0,
            note: None,
            last_log_line: None,
            last_log_lines: Vec::new(),
            launch_command: None,
            pid: None,
            artifact_size_bytes: None,
        }
    }

    #[must_use]
    pub fn with_local(mut self, local: bool) -> Self {
        self.local = local;
        self
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
        // The kind column carries a trailing `*` for a participant running
        // from a local directory (user-authored source, or a local path-pin
        // override) rather than a fetched catalog artifact - see
        // `ParticipantStatus::local`.
        let mut out = String::from(
            "participant                 kind          state       restarts  note  last log\n",
        );
        out.push_str(
            "--------------------------------------------------------------------------------\n",
        );
        for participant in self.participants.values() {
            let note = participant.note.as_deref().unwrap_or("-");
            let last = participant.last_log_line.as_deref().unwrap_or("-");
            let kind_label = format!(
                "{}{}",
                participant.kind.label(),
                if participant.local { "*" } else { "" }
            );
            out.push_str(&format!(
                "{:<27} {:<13} {:<11} {:>8}  {}  {}\n",
                trim_cell(&participant.id, 27),
                kind_label,
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

/// Where a routed log line came from - the ROUTING metadata a later TUI/logger
/// dedups on instead of comparing line text (see [`BoardBackend::route_log`]
/// and [`RoutedLogLine`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogSource {
    /// A structured `logs/{participant_id}` bus event
    /// ([`bus_log_subscriber_loop`]) - the primary source once a participant
    /// is far enough along to publish on the bus.
    Bus,
    /// A captured stdout/stderr line from the supervised child process
    /// ([`spawn_output_reader`]) - the only source before the bus connects
    /// (panics, pre-`#[setup]` startup), otherwise redundant with `Bus`.
    Raw,
}

/// Severity retained with routed logs so the global Logs page can filter
/// structured events without parsing their rendered text. Raw child output
/// has no typed level and is conservatively recorded as Info.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum LogSeverity {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

impl From<api::logs::Level> for LogSeverity {
    fn from(level: api::logs::Level) -> Self {
        match level {
            api::logs::Level::Trace => Self::Trace,
            api::logs::Level::Debug => Self::Debug,
            api::logs::Level::Info => Self::Info,
            api::logs::Level::Warn => Self::Warn,
            api::logs::Level::Error => Self::Error,
        }
    }
}

/// One routed log line, broadcast to [`BoardBackend::set_log_sink`]'s
/// subscriber (a live TUI) in addition to the bounded 8-line history kept on
/// the board itself. Kept separate from [`ParticipantStatus::last_log_lines`]
/// so a full-scrollback consumer (the TUI's own bounded ring buffer, see
/// `crate::tui::logs`) never has to poll a snapshot for log history.
#[derive(Debug, Clone)]
pub struct RoutedLogLine {
    pub participant: String,
    pub source: LogSource,
    pub severity: LogSeverity,
    pub text: String,
}

#[derive(Debug, Clone, Default)]
pub struct BoardBackend {
    inner: Arc<Mutex<BoardSnapshot>>,
    /// Wall-clock instant each participant's presence/heartbeat was last
    /// observed. Separate from `inner` because it is process-local bookkeeping
    /// (an `Instant`, not serializable) rather than board state; an entry
    /// appears here from the FIRST heartbeat of any readiness (including the
    /// pre-`#[setup]` `Initializing` beacon), so this alone cannot tell "never
    /// checked in" apart from "checked in once and then went silent mid-setup"
    /// - `ready_once` (below) makes that distinction.
    heartbeats: Arc<Mutex<BTreeMap<String, Instant>>>,
    /// Participant ids observed at `Readiness::Ready` at least once during
    /// their current incarnation. `mark_stale_heartbeats` only applies its 5s
    /// liveness sweep to ids in this set: a participant that has never reached
    /// `Ready` is still within a legitimate `#[setup]` (the runner publishes
    /// only one `Initializing` heartbeat before `#[setup]` runs, then nothing
    /// until the post-setup `Ready` beacon, so a slow setup is heartbeat-silent
    /// by design) and is bounded by its startup stage's timeout instead, not
    /// this sweep. Cleared alongside `heartbeats` by
    /// `reset_participant_liveness` on every (re)spawn so a fresh incarnation
    /// must earn `Ready` again before the sweep applies to it.
    ready_once: Arc<Mutex<BTreeSet<String>>>,
    /// Participants whose process lifecycle belongs to an external owner
    /// (currently Webots). Their heartbeat loss is observational: it degrades
    /// the board entry but cannot become session-teardown authority, and a
    /// later heartbeat may restore the current readiness.
    recoverable_presence: Arc<Mutex<BTreeSet<String>>>,
    /// Optional live sink for [`RoutedLogLine`]s - set by
    /// `session::controller::SessionController::drive_supervision` once its
    /// `TuiDisplay` renderer exists, so it can maintain its own bounded
    /// per-runtime scrollback (Part 3) without polling the board's own
    /// 8-line history. `None` outside a TUI session (Plain/Json output, or
    /// any test) - [`Self::route_log`] simply skips the broadcast in that
    /// case. Bounded: the consuming `TuiDisplay::redraw` already keeps its
    /// own bounded ring per runtime (`stores::log_store::LogStore`), so a
    /// full channel here just means a redraw is overdue - `route_log` drops
    /// the newest line rather than blocking the log-subscriber task that
    /// calls it.
    log_sink: Arc<Mutex<Option<mpsc::Sender<RoutedLogLine>>>>,
}

impl BoardBackend {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record an observed `presence/heartbeat` for `id`, driving its board
    /// state from OBSERVED readiness rather than any spawn-time assumption.
    /// Ignored for a supervised participant already in a terminal state
    /// (`Failed`, `Stopped`) - a heartbeat that was in flight when the process
    /// was independently torn down must not resurrect it. Externally managed
    /// participants registered with [`Self::mark_presence_recoverable`] may
    /// recover from `Failed`/`Degraded` when their owner recreates them.
    pub fn record_heartbeat(&self, id: &str, readiness: api::presence::Readiness) {
        self.heartbeats
            .lock()
            .expect("heartbeat mutex poisoned")
            .insert(id.to_string(), Instant::now());
        if matches!(readiness, api::presence::Readiness::Ready) {
            self.ready_once
                .lock()
                .expect("ready-once mutex poisoned")
                .insert(id.to_string());
        }
        let mut snapshot = self.inner.lock().expect("board mutex poisoned");
        let status = snapshot
            .participants
            .entry(id.to_string())
            .or_insert_with(|| {
                ParticipantStatus::new(id, ParticipantKind::Service, ParticipantState::Starting)
            });
        let recoverable = self
            .recoverable_presence
            .lock()
            .expect("recoverable presence mutex poisoned")
            .contains(id);
        if status.state == ParticipantState::Stopped
            || (status.state == ParticipantState::Failed && !recoverable)
        {
            return;
        }
        let was_unobserved = recoverable
            && matches!(
                status.state,
                ParticipantState::Degraded | ParticipantState::Failed
            );
        status.state = match readiness {
            api::presence::Readiness::Ready => ParticipantState::Ready,
            api::presence::Readiness::Degraded => ParticipantState::Degraded,
            api::presence::Readiness::Failed => ParticipantState::Failed,
            api::presence::Readiness::NotStarted | api::presence::Readiness::Initializing => {
                ParticipantState::Starting
            }
        };
        if was_unobserved
            && matches!(
                status.state,
                ParticipantState::Ready | ParticipantState::Starting
            )
        {
            status.note = Some("Webots-managed participant observed again".to_string());
        }
    }

    /// Mark `id` as externally managed presence. Its silence is recoverable
    /// observation state, not proof that the developer session must end.
    pub fn mark_presence_recoverable(&self, id: impl Into<String>) {
        self.recoverable_presence
            .lock()
            .expect("recoverable presence mutex poisoned")
            .insert(id.into());
    }

    /// Clear a participant's staleness bookkeeping - its last-heartbeat
    /// timestamp and its "has been `Ready`" bit - so a fresh incarnation
    /// starts with a clean liveness clock and must earn `Ready` again before
    /// `mark_stale_heartbeats` will apply to it. Called from `spawn_child` on
    /// every (re)spawn: the initial spawn, a crash restart, a `swap`, and a
    /// `resume` after `release` all funnel through it. Without this, a
    /// previously-`Ready`-then-crashed participant's stale pre-crash timestamp
    /// would survive the reset to `Starting` and could immediately re-`Fail`
    /// the freshly respawned process before it had a chance to publish its
    /// first heartbeat.
    pub fn reset_participant_liveness(&self, id: &str) {
        self.heartbeats
            .lock()
            .expect("heartbeat mutex poisoned")
            .remove(id);
        self.ready_once
            .lock()
            .expect("ready-once mutex poisoned")
            .remove(id);
    }

    /// Mark any participant that has reached `Ready` at least once this
    /// incarnation and has gone silent for longer than `stale_after`.
    /// CLI-supervised participants become `Failed`; externally managed
    /// participants become recoverably `Degraded` because pause/deletion is
    /// owned by Webots and may be followed by recreation.
    ///
    /// Deliberately excludes a participant that has never been observed
    /// `Ready` (only ever `Starting`, e.g. mid a legitimately slow
    /// `#[setup]`) - see the `ready_once` field docs. That case is bounded by
    /// its startup stage's timeout, not this sweep.
    pub fn mark_stale_heartbeats(&self, stale_after: Duration) {
        let now = Instant::now();
        let stale_ids: Vec<(String, bool)> = {
            let heartbeats = self.heartbeats.lock().expect("heartbeat mutex poisoned");
            let ready_once = self.ready_once.lock().expect("ready-once mutex poisoned");
            let recoverable = self
                .recoverable_presence
                .lock()
                .expect("recoverable presence mutex poisoned");
            heartbeats
                .iter()
                .filter(|(id, seen)| {
                    ready_once.contains(id.as_str()) && now.duration_since(**seen) > stale_after
                })
                .map(|(id, _)| (id.clone(), recoverable.contains(id)))
                .collect()
        };
        if stale_ids.is_empty() {
            return;
        }
        let mut snapshot = self.inner.lock().expect("board mutex poisoned");
        for (id, recoverable) in stale_ids {
            let Some(status) = snapshot.participants.get_mut(&id) else {
                continue;
            };
            if matches!(
                status.state,
                ParticipantState::Failed | ParticipantState::Stopped
            ) {
                continue;
            }
            if recoverable {
                status.state = ParticipantState::Degraded;
                status.note = Some(format!(
                    "Webots-managed participant not observed for over {}; waiting for its owner",
                    crate::human::duration(stale_after)
                ));
            } else {
                status.state = ParticipantState::Failed;
                status.note = Some(format!(
                    "heartbeat stopped: no presence/heartbeat observed for over {}",
                    crate::human::duration(stale_after)
                ));
            }
        }
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
            if matches!(
                state,
                ParticipantState::Starting
                    | ParticipantState::Restarting
                    | ParticipantState::Failed
                    | ParticipantState::Stopped
            ) {
                status.pid = None;
                status.artifact_size_bytes = None;
            }
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
                ParticipantStatus::new(id, ParticipantKind::Service, ParticipantState::Ready)
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
                ParticipantStatus::new(id, ParticipantKind::Service, ParticipantState::Ready)
            });
        status.last_log_line = Some(line.clone());
        status.last_log_lines.push(line);
        const MAX_LAST_LINES: usize = 8;
        if status.last_log_lines.len() > MAX_LAST_LINES {
            let drop_count = status.last_log_lines.len() - MAX_LAST_LINES;
            status.last_log_lines.drain(0..drop_count);
        }
    }

    /// Register the live [`RoutedLogLine`] sink for this session's display
    /// (a TUI). Replaces any previous sink - only one live display exists per
    /// session.
    pub fn set_log_sink(&self, sender: mpsc::Sender<RoutedLogLine>) {
        *self.log_sink.lock().expect("log sink mutex poisoned") = Some(sender);
    }

    /// Record a log line from a known routing source: updates the board's own
    /// bounded 8-line history exactly like [`Self::append_log`] (so
    /// `--message-format json`'s `status` snapshot and the legacy board model
    /// stay unchanged), then additionally forwards it to the live sink
    /// registered by [`Self::set_log_sink`], if any. This is the single
    /// funnel both [`bus_log_subscriber_loop`] and [`spawn_output_reader`]
    /// use, so a live TUI dedups by ROUTING (which of the two called this)
    /// rather than by comparing rendered text - see [`LogSource`].
    pub fn route_log(&self, id: &str, source: LogSource, text: impl Into<String>) {
        self.route_log_with_severity(id, source, LogSeverity::Info, text);
    }

    pub fn route_log_with_severity(
        &self,
        id: &str,
        source: LogSource,
        severity: LogSeverity,
        text: impl Into<String>,
    ) {
        let text = text.into();
        self.append_log(id, text.clone());
        let sink = self.log_sink.lock().expect("log sink mutex poisoned");
        if let Some(sender) = sink.as_ref() {
            // Non-blocking: a full channel (redraw overdue) or a closed one
            // (no live TUI) both just mean this line never reaches the
            // scrollback - never worth blocking the caller (a bus-log
            // subscriber or output-reader task) over.
            let _ = sender.try_send(RoutedLogLine {
                participant: id.to_string(),
                source,
                severity,
                text,
            });
        }
    }

    pub fn set_launch_command(&self, id: &str, command: ParticipantLaunchCommand) {
        let mut snapshot = self.inner.lock().expect("board mutex poisoned");
        if let Some(status) = snapshot.participants.get_mut(id) {
            status.launch_command = Some(command);
        }
    }

    pub fn set_process_details(
        &self,
        id: &str,
        pid: Option<u32>,
        artifact_size_bytes: Option<u64>,
    ) {
        let mut snapshot = self.inner.lock().expect("board mutex poisoned");
        if let Some(status) = snapshot.participants.get_mut(id) {
            status.pid = pid;
            status.artifact_size_bytes = artifact_size_bytes;
        }
    }

    #[must_use]
    pub fn snapshot(&self) -> BoardSnapshot {
        self.inner.lock().expect("board mutex poisoned").clone()
    }

    /// Session-only receive instants for the TUI. This stays outside the
    /// persisted BoardSnapshot and its stable plain/JSON representations.
    #[must_use]
    pub fn heartbeat_snapshot(&self) -> BTreeMap<String, Instant> {
        self.heartbeats
            .lock()
            .expect("heartbeat mutex poisoned")
            .clone()
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
    pub process_group: bool,
    pub note: Option<String>,
    /// Whether this participant is a checked phoxal bus participant that
    /// publishes its own `presence/heartbeat` (true for essentially every
    /// spec - services, drivers, and site tools built on the shared runner).
    /// `false` only for a process with no participant heartbeat of its own:
    /// the router (readiness is a separate CLI-owned bus probe) and the Webots
    /// application (process-lifecycle readiness). Drives whether the
    /// supervisor waits for an observed heartbeat before marking the process
    /// `Ready`.
    pub bus_participant: bool,
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

/// Finding A7/C2: this struct deliberately carries no UI/telemetry handle.
/// Live telemetry (host/process/router/joypad feeds) is owned by the caller
/// and passed directly to
/// `session::controller::SessionController::drive_supervision`, and this
/// loop never reads it - so an earlier `telemetry: TelemetryBackend` field
/// here was dead weight, not a real dependency.
#[derive(Debug)]
pub struct SupervisorOptions {
    pub restart_policy: RestartPolicy,
    pub action_rx: Option<mpsc::Receiver<SupervisorAction>>,
    pub requested_stop: Option<RequestedStop>,
    /// The session's root cancellation signal (`session::SessionController`
    /// owns the sender half): a Ctrl-C observed by the controller cancels
    /// this, and this loop selects on it directly instead of its own private
    /// `tokio::signal::ctrl_c()` - the controller is the ONE place that
    /// decides what Ctrl-C means (first = cancel + orderly teardown, second =
    /// force exit), never this loop. Defaults to a fresh, never-cancelled
    /// token so a caller that does not care about cancellation (every test in
    /// this module) does not have to construct one.
    pub token: tokio_util::sync::CancellationToken,
    /// Where this loop emits `SessionEvent`s (stage started/finished) for a
    /// live `SessionController` to render - see `crate::session::event`.
    /// `None` for a caller with no renderer to feed (every test in this
    /// module).
    pub events: Option<mpsc::Sender<crate::session::event::SessionEvent>>,
    /// Whether staged startup completion should transition the session to
    /// `SessionState::Running`. When `true`, once
    /// every stage has spawned and been observed ready with nothing left
    /// pending, this loop emits `SessionEvent::StagedStartupComplete` itself
    /// instead of the caller claiming `Running` before the supervisor even
    /// exists. Both host and simulation sessions enable this; simulation
    /// clock telemetry is not a lifecycle authority.
    pub emits_running_on_startup_complete: bool,
}

impl Default for SupervisorOptions {
    fn default() -> Self {
        Self {
            restart_policy: RestartPolicy::default(),
            action_rx: None,
            requested_stop: None,
            token: tokio_util::sync::CancellationToken::new(),
            events: None,
            emits_running_on_startup_complete: false,
        }
    }
}

#[derive(Debug)]
pub struct RequestedStop {
    participant_id: String,
    grace: Duration,
}

impl RequestedStop {
    pub fn new(participant_id: impl Into<String>, grace: Duration) -> Self {
        Self {
            participant_id: participant_id.into(),
            grace,
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
    /// Stop and respawn a participant from its own current spec, unchanged -
    /// the TUI's `r restart` (see `crate::display::DisplayAction::Restart`).
    /// Handled the same way as `Swap` with the participant's own spec cloned
    /// back in, rather than a new field on `RunningParticipant`, so it reuses
    /// the exact same stop/spawn/board-note sequence a hot-reload swap
    /// already goes through.
    Restart { id: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SupervisorOutcome {
    pub failed_participants: Vec<String>,
}

impl SupervisorOutcome {
    #[must_use]
    #[cfg(test)]
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
        };
        running.spawn_child(board).await?;
        Ok(running)
    }

    async fn spawn_child(&mut self, board: &BoardBackend) -> Result<()> {
        // Bug 2 fix: every (re)spawn - first spawn, crash restart, `swap`,
        // `resume` - resets board STATE to `Starting` below; it must also
        // reset the staleness bookkeeping the same instant, or a stale
        // pre-crash heartbeat timestamp can immediately re-`Fail` this fresh
        // incarnation before it gets a chance to check in.
        board.reset_participant_liveness(&self.spec.id);
        board.set_state(
            &self.spec.id,
            ParticipantState::Starting,
            self.spec.note.clone(),
        );
        let mut command = Command::new(&self.spec.executable);
        command.args(&self.spec.args);
        #[cfg(unix)]
        if self.spec.process_group {
            command.process_group(0);
        }
        if let Some(cwd) = &self.spec.cwd {
            command.current_dir(cwd);
        }
        command
            .envs(self.spec.env.iter().map(|(key, value)| (key, value)))
            // No terminal input for any child: the TUI (Part 4) owns terminal
            // input exclusively (there is no Interact/raw-io tab), and a
            // child that inherited this process's stdin could otherwise race
            // it for keystrokes.
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command
            .spawn()
            .with_context(|| format!("failed to spawn {}", self.spec.command_line()))?;
        let pid = child.id();
        let artifact_size_bytes = fs::metadata(&self.spec.executable)
            .ok()
            .filter(|metadata| metadata.is_file())
            .map(|metadata| metadata.len());
        board.set_process_details(&self.spec.id, pid, artifact_size_bytes);
        let pid = pid.unwrap_or_default();
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
        if self.spec.bus_participant {
            // OBSERVED readiness (not spawn-is-ready): a bus participant stays
            // `Starting` (set at the top of this function) until the
            // supervisor's presence/heartbeat subscriber observes its own
            // heartbeat go `Ready` - see `BoardBackend::record_heartbeat`. A
            // process that spawned successfully but never gets that far
            // (crashed before `#[setup]` completed, hung, or was silently
            // never launched by Webots) must never be reported ready.
        } else {
            // No bus identity to observe (e.g. the Webots application itself,
            // see `ParticipantSpec::bus_participant`) - readiness is
            // necessarily process-lifecycle only.
            board.set_state(
                &self.spec.id,
                ParticipantState::Ready,
                self.spec.note.clone(),
            );
        }
        Ok(())
    }

    fn record_spawn_failure(
        &mut self,
        board: &BoardBackend,
        operation: &str,
        error: &anyhow::Error,
    ) {
        self.child = None;
        self.restart_at = None;
        self.failed = true;
        let detail = format!("{operation} failed: {error:#}");
        board.append_log(&self.spec.id, format!("supervisor: {detail}"));
        board.set_state(&self.spec.id, ParticipantState::Failed, Some(detail));
    }

    async fn wait_for_requested_stop(
        &mut self,
        board: &BoardBackend,
        budget: Duration,
        terminate_sent: bool,
    ) -> Result<bool> {
        let Some(child) = self.child.as_mut() else {
            return Ok(false);
        };
        let status = match timeout(budget, child.wait()).await {
            Ok(status) => status.context("failed to wait for requested child stop")?,
            Err(_) => return Ok(false),
        };
        self.child = None;
        join_reader(self.stdout_task.take()).await;
        join_reader(self.stderr_task.take()).await;
        self.failed = true;
        if requested_stop_exit_is_clean(&status, terminate_sent) {
            board.set_state(
                &self.spec.id,
                ParticipantState::Stopped,
                Some(format!("stopped after requested SIGTERM ({status})")),
            );
        } else {
            board.set_state(
                &self.spec.id,
                ParticipantState::Failed,
                Some(format!(
                    "exited independently during requested stop ({status})"
                )),
            );
        }
        Ok(true)
    }

    async fn kill_process_group_after_timeout(&mut self, board: &BoardBackend) -> Result<()> {
        if !self.spec.process_group {
            bail!(
                "requested-stop fallback requires an isolated process group for {}",
                self.spec.id
            );
        }
        let Some(mut child) = self.child.take() else {
            return Ok(());
        };
        board.append_log(
            &self.spec.id,
            "supervisor: SIGTERM grace expired; killing process group",
        );
        if let Err(error) = kill_child_process_group(&mut child).await {
            self.child = Some(child);
            return Err(error);
        }
        join_reader(self.stdout_task.take()).await;
        join_reader(self.stderr_task.take()).await;
        self.failed = true;
        board.set_state(
            &self.spec.id,
            ParticipantState::Failed,
            Some("SIGTERM grace expired; SIGKILL fallback used".to_string()),
        );
        Ok(())
    }

    async fn poll(&mut self, board: &BoardBackend, policy: &RestartPolicy) -> Result<()> {
        if self.failed {
            return Ok(());
        }
        if let Some(restart_at) = self.restart_at {
            if Instant::now() < restart_at {
                return Ok(());
            }
            self.restart_at = None;
            if let Err(error) = self.spawn_child(board).await {
                self.record_spawn_failure(board, "restart spawn", &error);
            }
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
                    "StartLimitBurst exhausted after {} failures in {}; last status {status}",
                    policy.start_limit_burst,
                    crate::human::duration(policy.start_limit_interval)
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
                "exited with {status}; restarting in {}",
                crate::human::duration(policy.restart_delay)
            )),
        );
        self.restart_at = Some(now + policy.restart_delay);
        Ok(())
    }

    #[cfg(test)]
    fn is_active(&self) -> bool {
        !self.failed && (self.child.is_some() || self.restart_at.is_some())
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
        self.restart_at = None;
        if let Err(error) = self.spawn_child(board).await {
            self.record_spawn_failure(board, "swap spawn", &error);
            return Ok(());
        }
        // `spawn_child` already applied the observed-readiness state (Starting
        // for a bus participant, Ready for a process-only one); attach the
        // swap note without overriding whichever state it landed on.
        board.set_note(&self.spec.id, note);
        Ok(())
    }
}

fn requested_stop_exit_is_clean(status: &std::process::ExitStatus, terminate_sent: bool) -> bool {
    if !terminate_sent {
        return false;
    }
    if status.success() {
        return true;
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        status.signal() == Some(libc::SIGTERM)
    }
    #[cfg(not(unix))]
    {
        false
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
                Ok(Some(line)) => {
                    board.route_log(&id, LogSource::Raw, format!("{stream}: {line}"));
                }
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

/// One stage of a mode-specific staged startup: the participants to spawn at
/// this stage, plus every board id (spawned here or not - a
/// SIMULATION-MANAGED participant with no `ParticipantSpec` of its own can
/// still gate a later stage) that must reach `Ready` before the next stage's
/// members are spawned. `run`'s stages are `router < other tools < drivers <
/// services`; `simulation run`'s are `router < other tools < Webots app <
/// simulation supervisor (wait-only) < services < robot/controllers
/// (wait-only)`. There is deliberately no clock stage.
#[derive(Debug, Clone, Default)]
pub struct SupervisionStage {
    /// Human-readable name for this stage, used in the stalled-stage error
    /// message and as the `PhaseId`/label of the `SessionEvent::PhaseStarted`/
    /// `PhaseFinished` pair this stage emits (see `spawn_stage_emitting`).
    pub label: String,
    pub specs: Vec<ParticipantSpec>,
    /// Board ids that must be observed `Ready` before the next stage spawns.
    /// Defaults to every spawned spec's own id that is a bus participant
    /// (see [`Self::new`]); extend with [`Self::with_extra_ready_ids`] for a
    /// wait-only id that has no `ParticipantSpec` of its own.
    pub ready_ids: Vec<String>,
    /// Spawned processes whose terminal failure aborts this stage even when
    /// readiness is established by a separate probe (notably tool-router).
    pub failure_ids: Vec<String>,
    pub router_bus_probe: Option<RouterBusProbe>,
    #[cfg(test)]
    manual_router_readiness: Option<Arc<AtomicBool>>,
    pub timeout: crate::session::output::WaitBudget,
}

#[derive(Debug, Clone)]
pub struct RouterBusProbe {
    pub namespace: String,
    pub robot_id: String,
    pub connect: String,
}

impl SupervisionStage {
    #[must_use]
    pub fn new(
        label: impl Into<String>,
        specs: Vec<ParticipantSpec>,
        timeout: crate::session::output::WaitBudget,
    ) -> Self {
        let ready_ids = specs
            .iter()
            .filter(|spec| spec.bus_participant)
            .map(|spec| spec.id.clone())
            .collect();
        let failure_ids = specs.iter().map(|spec| spec.id.clone()).collect();
        Self {
            label: label.into(),
            specs,
            ready_ids,
            failure_ids,
            router_bus_probe: None,
            #[cfg(test)]
            manual_router_readiness: None,
            timeout,
        }
    }

    #[must_use]
    pub fn with_extra_ready_ids(mut self, ids: impl IntoIterator<Item = String>) -> Self {
        self.ready_ids.extend(ids);
        self
    }

    #[must_use]
    pub fn with_router_bus_probe(
        mut self,
        namespace: impl Into<String>,
        robot_id: impl Into<String>,
        connect: impl Into<String>,
    ) -> Self {
        self.router_bus_probe = Some(RouterBusProbe {
            namespace: namespace.into(),
            robot_id: robot_id.into(),
            connect: connect.into(),
        });
        self
    }

    #[cfg(test)]
    #[must_use]
    fn with_manual_router_readiness(mut self, ready: Arc<AtomicBool>) -> Self {
        self.manual_router_readiness = Some(ready);
        self
    }
}

async fn spawn_stage(
    running: &mut Vec<RunningParticipant>,
    board: &BoardBackend,
    specs: Vec<ParticipantSpec>,
) {
    for spec in specs {
        let id = spec.id.clone();
        match RunningParticipant::spawn(spec, board).await {
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
}

/// A stage currently being waited on: its expected ready ids, its deadline,
/// and when it started (so the eventual `PhaseFinished` can report real
/// elapsed time) - the loop's replacement for a raw
/// `(String, Vec<String>, Instant)` tuple, which the finished-elapsed
/// bookkeeping outgrew.
struct PendingStage {
    label: String,
    ready_ids: Vec<String>,
    failure_ids: Vec<String>,
    router_bus_readiness: Option<RouterBusReadiness>,
    /// `None` for an unbounded wait (Product decision 6/finding D2) - there is
    /// no `Instant` to ever compare against.
    deadline: Option<Instant>,
    started: Instant,
}

#[derive(Debug)]
struct RouterBusReadiness {
    ready: Arc<AtomicBool>,
    cancel: Option<Arc<AtomicBool>>,
}

impl RouterBusReadiness {
    fn is_ready(&self) -> bool {
        self.ready.load(Ordering::Acquire)
    }
}

impl Drop for RouterBusReadiness {
    fn drop(&mut self) {
        if let Some(cancel) = &self.cancel {
            cancel.store(true, Ordering::Release);
        }
    }
}

/// Send `event` to `events`, if a `SessionController` is listening. Awaits
/// the send so a lifecycle/control transition (a phase or session-state
/// change) can never be silently dropped under channel backpressure (finding
/// B5) - only [`crate::session::diagnostics`]'s much higher-volume, lower-
/// severity telemetry/log routing keeps a non-blocking `try_send`. The
/// channel is generously bounded (`EVENT_CHANNEL_CAPACITY`) and the
/// controller's own select loop drains it continuously, so this resolves
/// promptly under normal operation; a truly gone/closed receiver (the
/// controller already tore down) makes the send fail immediately rather than
/// hang, which is why the result is still discarded here.
async fn emit_event(
    events: Option<&mpsc::Sender<crate::session::event::SessionEvent>>,
    event: crate::session::event::SessionEvent,
) {
    if let Some(sender) = events {
        let _ = sender.send(event).await;
    }
}

/// Spawn one stage's participants (if it has any work at all - Product
/// decision 3: never emit a phase for a stage with nothing to start) and, when
/// `events` is wired, emit its `PhaseStarted` immediately and `PhaseFinished`
/// too IF the stage has nothing further to wait for (`ready_ids` empty and no
/// router bus probe). A stage that DOES have readiness work returns its own
/// [`PendingStage`] instead; the caller's own loop emits its `PhaseFinished`
/// once `await_participants_ready` resolves.
async fn spawn_stage_emitting(
    running: &mut Vec<RunningParticipant>,
    board: &BoardBackend,
    events: Option<&mpsc::Sender<crate::session::event::SessionEvent>>,
    stage: SupervisionStage,
) -> Option<PendingStage> {
    let SupervisionStage {
        label,
        specs,
        ready_ids,
        failure_ids,
        router_bus_probe,
        #[cfg(test)]
        manual_router_readiness,
        timeout: stage_timeout,
    } = stage;
    #[cfg(test)]
    let has_router_probe = router_bus_probe.is_some() || manual_router_readiness.is_some();
    #[cfg(not(test))]
    let has_router_probe = router_bus_probe.is_some();
    if specs.is_empty() && ready_ids.is_empty() && !has_router_probe {
        return None;
    }
    let started = Instant::now();
    emit_event(
        events,
        crate::session::event::SessionEvent::PhaseStarted {
            id: crate::session::event::PhaseId::new(label.clone()),
            label: label.clone(),
        },
    )
    .await;
    spawn_stage(running, board, specs).await;
    #[cfg(test)]
    let router_bus_readiness = manual_router_readiness
        .map(|ready| RouterBusReadiness {
            ready,
            cancel: None,
        })
        .or_else(|| router_bus_probe.map(start_router_bus_probe));
    #[cfg(not(test))]
    let router_bus_readiness = router_bus_probe.map(start_router_bus_probe);
    if ready_ids.is_empty() && router_bus_readiness.is_none() {
        emit_event(
            events,
            crate::session::event::SessionEvent::PhaseFinished {
                id: crate::session::event::PhaseId::new(label.clone()),
                outcome: crate::session::event::PhaseOutcome::Succeeded,
                elapsed: started.elapsed(),
            },
        )
        .await;
        return None;
    }
    Some(PendingStage {
        label,
        ready_ids,
        failure_ids,
        router_bus_readiness,
        deadline: stage_timeout.deadline_from(Instant::now()),
        started,
    })
}

async fn spawn_until_pending(
    running: &mut Vec<RunningParticipant>,
    board: &BoardBackend,
    events: Option<&mpsc::Sender<crate::session::event::SessionEvent>>,
    stage_queue: &mut VecDeque<SupervisionStage>,
) -> Option<PendingStage> {
    while let Some(stage) = stage_queue.pop_front() {
        if let Some(pending) = spawn_stage_emitting(running, board, events, stage).await {
            return Some(pending);
        }
    }
    None
}

/// Wait until every id in `stage_ids` has been OBSERVED `Ready` on the
/// board, or `timeout` elapses. Shared by host and simulation staged startup;
/// returns `Ok(())` on
/// success with no side effects; on an explicit terminal `Failed` readiness
/// it returns `Err` immediately (never waits out the timeout for a graph
/// that already ended unhealthy); on timeout it marks every still-missing id
/// `Failed` on the board (so `SupervisorOutcome::graph_healthy` reflects the
/// stall) and returns `Err` naming exactly what never showed up.
#[cfg(test)]
pub async fn await_participants_ready(
    board: &BoardBackend,
    stage_ids: &[String],
    budget: crate::session::output::WaitBudget,
    poll_interval: Duration,
) -> Result<()> {
    await_stage_ready(board, stage_ids, stage_ids, None, budget, poll_interval).await
}

async fn await_stage_ready(
    board: &BoardBackend,
    ready_ids: &[String],
    failure_ids: &[String],
    router_bus_readiness: Option<&RouterBusReadiness>,
    budget: crate::session::output::WaitBudget,
    poll_interval: Duration,
) -> Result<()> {
    if ready_ids.is_empty() && router_bus_readiness.is_none() {
        return Ok(());
    }
    let started = Instant::now();
    let deadline = budget.deadline_from(started);
    let mut interval = tokio::time::interval(poll_interval);
    interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
    loop {
        interval.tick().await;
        let snapshot = board.snapshot();
        let failed = failed_expected_participants(&snapshot, failure_ids);
        if !failed.is_empty() {
            bail!(
                "stage ended unhealthy; failed participants: {}",
                failed.join(", ")
            );
        }
        let missing = missing_ready_participants(&snapshot, ready_ids);
        let router_ready = match router_bus_readiness {
            Some(readiness) => readiness.is_ready(),
            None => true,
        };
        if missing.is_empty() && router_ready {
            return Ok(());
        }
        // `deadline` is `None` for an unbounded wait (Product decision 6) -
        // there is nothing to ever compare `Instant::now()` against, so a
        // missing participant simply keeps waiting for as long as the
        // operator leaves the session open.
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            let waited = crate::human::duration(started.elapsed());
            for id in &missing {
                board.set_state(
                    id,
                    ParticipantState::Failed,
                    Some(format!(
                        "stage readiness timed out after {waited}: never observed ready"
                    )),
                );
            }
            if !router_ready && let Some(router_id) = failure_ids.first() {
                board.set_state(
                    router_id,
                    ParticipantState::Failed,
                    Some(format!(
                        "stage readiness timed out after {waited}: CLI could not open the router bus"
                    )),
                );
            }
            let mut stalled = missing;
            if !router_ready {
                stalled.push("router bus connection".to_string());
            }
            bail!(
                "stage readiness timed out after {waited}: participant(s) never observed ready: {}",
                stalled.join(", ")
            );
        }
    }
}

fn start_router_bus_probe(probe: RouterBusProbe) -> RouterBusReadiness {
    let ready = Arc::new(AtomicBool::new(false));
    let cancel = Arc::new(AtomicBool::new(false));
    let ready_for_thread = Arc::clone(&ready);
    let cancel_for_thread = Arc::clone(&cancel);
    std::thread::spawn(move || {
        let runtime = match tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
        {
            Ok(runtime) => runtime,
            Err(_) => return,
        };
        while !cancel_for_thread.load(Ordering::Acquire) {
            // A TCP preflight is only a warning-suppression optimization; the
            // stage remains gated on the successful Bus::open below.
            if !endpoint_reachable(&probe.connect, Duration::from_millis(50)) {
                std::thread::sleep(Duration::from_millis(100));
                continue;
            }
            let opened = runtime.block_on(Bus::open(BusConfig {
                namespace: probe.namespace.clone(),
                robot_id: probe.robot_id.clone(),
                participant: "phoxal-cli-router-readiness".to_string(),
                incarnation: 0,
                connect_endpoints: vec![probe.connect.clone()],
            }));
            if let Ok(bus) = opened {
                let _ = runtime.block_on(bus.close());
                ready_for_thread.store(true, Ordering::Release);
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    });
    RouterBusReadiness {
        ready,
        cancel: Some(cancel),
    }
}

/// Emit `SessionEvent::StagedStartupComplete` exactly when there is truly
/// nothing left to spawn or wait for (finding B3) - `pending_stage.is_none()`
/// after a stage-draining loop means the loop only stopped because the queue
/// was genuinely empty, not because it parked on a stage awaiting readiness
/// (a park always sets `pending_stage`). Only fires for a session that opted
/// in via `emits_running_on_startup_complete`.
async fn maybe_emit_staged_startup_complete(
    options: &SupervisorOptions,
    events: Option<&mpsc::Sender<crate::session::event::SessionEvent>>,
    pending_stage: &Option<PendingStage>,
) {
    if options.emits_running_on_startup_complete && pending_stage.is_none() {
        emit_event(
            events,
            crate::session::event::SessionEvent::StagedStartupComplete,
        )
        .await;
    }
}

pub async fn supervise_until_shutdown(
    stages: Vec<SupervisionStage>,
    board: BoardBackend,
    mut options: SupervisorOptions,
) -> Result<SupervisorOutcome> {
    let mut running = Vec::new();
    let mut stage_queue: VecDeque<SupervisionStage> = stages.into();
    let token = options.token.clone();
    let events_tx = options.events.take();

    // Spawn every leading stage that has nothing to wait for back-to-back
    // (uncommon in practice - every real stage waits on at least the router
    // or a heartbeat - but keeps a zero-wait stage from stalling a whole
    // startup on an empty `select!` branch), then park on the first stage
    // that actually gates the next one.
    let mut pending_stage =
        spawn_until_pending(&mut running, &board, events_tx.as_ref(), &mut stage_queue).await;
    maybe_emit_staged_startup_complete(&options, events_tx.as_ref(), &pending_stage).await;

    let mut ticker = tokio::time::interval(Duration::from_millis(500));
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut action_rx = options.action_rx.take();
    loop {
        tokio::select! {
            () = token.cancelled() => {
                break;
            }
            action = recv_action(&mut action_rx) => {
                if let Some(action) = action {
                    handle_action(&mut running, &board, action).await?;
                }
            }
            // Recreated fresh every loop pass (the same pattern as
            // `recv_action` above): the ONLY state that must survive a
            // cancelled poll is the stage's own deadline, which lives in
            // `pending_stage` outside this future, not inside it - so
            // recreating the await is safe and never resets the timeout.
            result = await_stage_ready(
                &board,
                pending_stage.as_ref().map_or(&[][..], |stage| stage.ready_ids.as_slice()),
                pending_stage.as_ref().map_or(&[][..], |stage| stage.failure_ids.as_slice()),
                pending_stage.as_ref().and_then(|stage| stage.router_bus_readiness.as_ref()),
                pending_stage.as_ref().map_or(WaitBudget::Unbounded, |stage| match stage.deadline {
                    Some(deadline) => WaitBudget::Bounded(deadline.saturating_duration_since(Instant::now())),
                    None => WaitBudget::Unbounded,
                }),
                Duration::from_millis(200),
            ), if pending_stage.is_some() => {
                let stage = pending_stage.take().expect("guarded by is_some");
                match result {
                    Ok(()) => {
                        board.append_log("supervisor", format!("supervisor: stage '{}' ready", stage.label));
                        emit_event(events_tx.as_ref(), crate::session::event::SessionEvent::PhaseFinished {
                            id: crate::session::event::PhaseId::new(stage.label.clone()),
                            outcome: crate::session::event::PhaseOutcome::Succeeded,
                            elapsed: stage.started.elapsed(),
                        }).await;
                        pending_stage = spawn_until_pending(
                            &mut running,
                            &board,
                            events_tx.as_ref(),
                            &mut stage_queue,
                        ).await;
                        maybe_emit_staged_startup_complete(&options, events_tx.as_ref(), &pending_stage).await;
                    }
                    Err(error) => {
                        let reason = format!("stage '{}' stalled: {error:#}", stage.label);
                        board.append_log(
                            "supervisor",
                            format!("supervisor: {reason}"),
                        );
                        emit_event(events_tx.as_ref(), crate::session::event::SessionEvent::PhaseFinished {
                            id: crate::session::event::PhaseId::new(stage.label.clone()),
                            outcome: crate::session::event::PhaseOutcome::Failed { error: format!("{error:#}") },
                            elapsed: stage.started.elapsed(),
                        }).await;
                        pending_stage = spawn_until_pending(
                            &mut running,
                            &board,
                            events_tx.as_ref(),
                            &mut stage_queue,
                        ).await;
                        maybe_emit_staged_startup_complete(
                            &options,
                            events_tx.as_ref(),
                            &pending_stage,
                        ).await;
                    }
                }
            }
            _ = ticker.tick() => {
                for participant in &mut running {
                    participant.poll(&board, &options.restart_policy).await?;
                }
                board.mark_stale_heartbeats(HEARTBEAT_STALE_TIMEOUT);
            }
        }
    }

    if let Some(requested_stop) = options.requested_stop.take() {
        request_participant_stop(&mut running, &board, requested_stop).await;
    }
    shutdown_all(&mut running, &board).await;
    Ok(SupervisorOutcome {
        failed_participants: board.snapshot().failed_participants(),
    })
}

async fn request_participant_stop(
    running: &mut [RunningParticipant],
    board: &BoardBackend,
    requested_stop: RequestedStop,
) {
    let Some(participant) = running
        .iter_mut()
        .find(|participant| participant.spec.id == requested_stop.participant_id)
    else {
        return;
    };
    if participant.child.is_none() {
        if participant.restart_at.take().is_some() {
            participant.failed = true;
            board.set_state(
                &participant.spec.id,
                ParticipantState::Failed,
                Some("crashed before requested stop while restart was pending".to_string()),
            );
        }
        return;
    }

    board.append_log(
        &participant.spec.id,
        "supervisor: sending SIGTERM for requested stop",
    );
    let Some(pid) = participant.child.as_ref().and_then(Child::id) else {
        board.set_state(
            &participant.spec.id,
            ParticipantState::Failed,
            Some("requested-stop child has no pid".to_string()),
        );
        return;
    };
    let terminate_sent = match send_terminate(pid).await {
        Ok(()) => {
            board.append_log(
                &participant.spec.id,
                "supervisor: SIGTERM sent; waiting for child exit",
            );
            true
        }
        Err(error) => {
            board.append_log(
                &participant.spec.id,
                format!("supervisor: failed to send SIGTERM; waiting before fallback: {error:#}"),
            );
            false
        }
    };

    match participant
        .wait_for_requested_stop(board, requested_stop.grace, terminate_sent)
        .await
    {
        Ok(true) => {}
        Ok(false) => {
            if let Err(error) = participant.kill_process_group_after_timeout(board).await {
                board.set_state(
                    &participant.spec.id,
                    ParticipantState::Failed,
                    Some(format!("process-group SIGKILL failed: {error:#}")),
                );
            }
        }
        Err(error) => {
            board.set_state(
                &participant.spec.id,
                ParticipantState::Failed,
                Some(format!("requested-stop wait failed: {error:#}")),
            );
        }
    }
}

async fn recv_action(
    action_rx: &mut Option<mpsc::Receiver<SupervisorAction>>,
) -> Option<SupervisorAction> {
    let action = match action_rx {
        Some(action_rx) => action_rx.recv().await,
        None => return std::future::pending().await,
    };
    if action.is_none() {
        action_rx.take();
    }
    action
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
        SupervisorAction::Restart { id } => {
            let Some(participant) = running
                .iter_mut()
                .find(|participant| participant.spec.id == id)
            else {
                board.append_log(&id, "supervisor: restart requested for unknown participant");
                return Ok(());
            };
            let spec = participant.spec.clone();
            participant
                .swap(spec, board, "manual restart".to_string())
                .await
        }
    }
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
        let _ = send_terminate(pid).await;
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

async fn kill_child_process_group(child: &mut Child) -> Result<()> {
    #[cfg(unix)]
    {
        let pid = child.id().context("process group leader has no pid")?;
        send_process_group_signal(pid, libc::SIGKILL)?;
        child
            .wait()
            .await
            .context("failed to wait for process group leader after SIGKILL")?;
        let kill_deadline = Instant::now() + Duration::from_secs(1);
        while process_group_alive(pid)? {
            if Instant::now() >= kill_deadline {
                bail!("child process group remained alive after SIGKILL");
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        child.start_kill().context("failed to kill child")?;
        child.wait().await.context("failed to wait after kill")?;
        Ok(())
    }
}

#[cfg(unix)]
fn send_process_group_signal(pid: u32, signal: libc::c_int) -> Result<()> {
    let process_group =
        i32::try_from(pid).context("child pid does not fit in a process-group id")?;
    if unsafe { libc::kill(-process_group, signal) } == 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        return Ok(());
    }
    Err(error).context("failed to signal child process group")
}

#[cfg(unix)]
fn process_group_alive(pid: u32) -> Result<bool> {
    let process_group =
        i32::try_from(pid).context("child pid does not fit in a process-group id")?;
    if unsafe { libc::kill(-process_group, 0) } == 0 {
        return Ok(true);
    }
    let error = std::io::Error::last_os_error();
    match error.raw_os_error() {
        Some(libc::ESRCH) => Ok(false),
        Some(libc::EPERM) => Ok(true),
        _ => Err(error).context("failed to inspect child process group"),
    }
}

async fn send_terminate(pid: u32) -> Result<()> {
    #[cfg(unix)]
    {
        let status = Command::new("kill")
            .arg("-TERM")
            .arg(pid.to_string())
            .status()
            .await
            .context("failed to invoke kill -TERM")?;
        if !status.success() {
            bail!("kill -TERM exited with {status}");
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        Ok(())
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

/// Wait for a TCP router endpoint before asking Zenoh to open a session.
/// Managed sessions intentionally start their observer feeds before the
/// router process so they cannot miss early readiness. A cheap TCP preflight
/// keeps those expected retries from producing Zenoh connection warnings on
/// top of the alternate-screen TUI.
pub(crate) async fn wait_for_endpoint(endpoint: &str) {
    while !endpoint_reachable(endpoint, Duration::from_millis(50)) {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
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

pub fn start_bus_log_subscriber(
    namespace: String,
    robot_id: String,
    connect: String,
    board: BoardBackend,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            wait_for_endpoint(&connect).await;
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
    let topic = Topic::<Subscribe<api::logs::Event>>::new_owned(logs_wildcard_topic_key());
    let subscriber = Subscriber::<api::logs::Event>::new(&bus, &topic, 128).await?;
    loop {
        let received = subscriber.recv().await?;
        let id = received.metadata.source.participant;
        board.route_log_with_severity(
            &id,
            LogSource::Bus,
            received.body.level.into(),
            render_log_event(&received.body),
        );
    }
}

/// The `logs/{participant_id}` contract's version-qualified wildcard key,
/// e.g. `v1/logs/*`. `logs::Event::TOPIC` (`ContractBody::TOPIC`) is the
/// per-participant literal `v1/logs/{participant_id}`, which is not
/// itself subscribable across every participant - building the key from
/// `ContractBody::VERSION` instead of hand-writing the version prefix
/// keeps this in lockstep with the api tree if the version ever changes.
#[must_use]
pub fn logs_wildcard_topic_key() -> String {
    format!(
        "{}/logs/*",
        <api::logs::Event as phoxal::bus::ContractBody>::VERSION
    )
}

/// Subscribe every participant's `presence/heartbeat` on one robot's bus and
/// drive the board's OBSERVED readiness from it (`BoardBackend::record_heartbeat`),
/// mirroring `start_bus_log_subscriber`. Unlike `logs/{participant_id}`,
/// `presence/heartbeat` is a single static (non-wildcarded) topic that every
/// participant publishes to, told apart only by `metadata.source.participant` -
/// see `phoxal-api`'s `presence` node.
pub fn start_presence_heartbeat_subscriber(
    namespace: String,
    robot_id: String,
    connect: String,
    board: BoardBackend,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            wait_for_endpoint(&connect).await;
            match presence_heartbeat_subscriber_loop(
                namespace.clone(),
                robot_id.clone(),
                connect.clone(),
                board.clone(),
            )
            .await
            {
                Ok(()) => break,
                Err(error) => {
                    tracing::debug!("presence heartbeat subscriber waiting for router: {error:#}");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
        }
    })
}

async fn presence_heartbeat_subscriber_loop(
    namespace: String,
    robot_id: String,
    connect: String,
    board: BoardBackend,
) -> Result<()> {
    let bus = Bus::open(BusConfig {
        namespace,
        robot_id,
        participant: "phoxal-cli-supervisor-presence".to_string(),
        incarnation: 0,
        connect_endpoints: vec![connect],
    })
    .await
    .map_err(|error| anyhow!("failed to open bus presence subscription: {error}"))?;
    let topic = Topic::<Subscribe<api::presence::Heartbeat>>::new_static(
        <api::presence::Heartbeat as phoxal::bus::ContractBody>::TOPIC,
    );
    let subscriber = Subscriber::<api::presence::Heartbeat>::new(&bus, &topic, 128).await?;
    loop {
        let received = subscriber.recv().await?;
        // The body carries `participant` too (redundant with the metadata
        // source), but `metadata.source.participant` is the framework-stamped
        // identity - the same field the log subscriber trusts - so prefer it.
        let id = received.metadata.source.participant;
        board.record_heartbeat(&id, received.body.readiness);
    }
}

/// One `v2::simulation::Clock` sample, as surfaced to the TUI top bar.
/// A new sample means the world advanced; silence means it did not.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ClockSample {
    pub now_ns: u64,
    pub step: u64,
}

/// Latest observed clock sample for the TUI. Clock telemetry is deliberately
/// not part of CLI readiness: the session becomes usable when its
/// participants are ready, regardless of whether a world step has occurred.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ClockObservation {
    pub latest: Option<ClockSample>,
}

/// Start a background feed of `v2::simulation::Clock` samples. Returns a
/// `watch::Receiver` the TUI's telemetry layer polls cheaply, plus the feed
/// task's handle.
pub fn start_clock_feed(
    namespace: String,
    robot_id: String,
    connect: String,
) -> (watch::Receiver<ClockObservation>, JoinHandle<()>) {
    let (tx, rx) = watch::channel(ClockObservation::default());
    let handle = tokio::spawn(async move {
        loop {
            wait_for_endpoint(&connect).await;
            match clock_feed_loop(namespace.clone(), robot_id.clone(), connect.clone(), &tx).await {
                Ok(()) => break,
                Err(error) => {
                    tracing::debug!("clock telemetry feed waiting for router: {error:#}");
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }
            }
        }
    });
    (rx, handle)
}

async fn clock_feed_loop(
    namespace: String,
    robot_id: String,
    connect: String,
    tx: &watch::Sender<ClockObservation>,
) -> Result<()> {
    let bus = Bus::open(BusConfig {
        namespace,
        robot_id,
        participant: "phoxal-cli-clock-observer".to_string(),
        incarnation: 0,
        connect_endpoints: vec![connect],
    })
    .await
    .map_err(|error| anyhow!("failed to open bus clock subscription: {error}"))?;
    let topic = Topic::<Subscribe<preview_api::simulation::Clock>>::new_static(
        <preview_api::simulation::Clock as phoxal::bus::ContractBody>::TOPIC,
    );
    let subscriber = Subscriber::<preview_api::simulation::Clock>::new(&bus, &topic, 32).await?;
    loop {
        let received = subscriber.recv().await?;
        tx.send_modify(|observation| {
            observation.latest = Some(ClockSample {
                now_ns: received.body.now_ns,
                step: received.body.step,
            });
        });
    }
}

/// Ids in `expected_bus_ids` not yet observed `Ready` on the board.
fn missing_ready_participants(board: &BoardSnapshot, expected_bus_ids: &[String]) -> Vec<String> {
    expected_bus_ids
        .iter()
        .filter(|id| {
            !board
                .participants
                .get(id.as_str())
                .is_some_and(|status| status.state == ParticipantState::Ready)
        })
        .cloned()
        .collect()
}

fn failed_expected_participants(board: &BoardSnapshot, expected_bus_ids: &[String]) -> Vec<String> {
    expected_bus_ids
        .iter()
        .filter(|id| {
            board
                .participants
                .get(id.as_str())
                .is_some_and(|status| status.state == ParticipantState::Failed)
        })
        .cloned()
        .collect()
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

    #[test]
    fn process_details_are_session_only_and_clear_with_the_incarnation() {
        let board = BoardBackend::new();
        board.upsert(ParticipantStatus::new(
            "motion",
            ParticipantKind::Service,
            ParticipantState::Ready,
        ));
        board.set_process_details("motion", Some(42), Some(1_024));

        let snapshot = board.snapshot();
        let status = snapshot.participants.get("motion").expect("motion");
        assert_eq!(status.pid, Some(42));
        assert_eq!(status.artifact_size_bytes, Some(1_024));
        let json = serde_json::to_value(&snapshot).expect("serialize board");
        let serialized = json.to_string();
        assert!(!serialized.contains("pid"));
        assert!(!serialized.contains("artifact_size_bytes"));

        board.set_state("motion", ParticipantState::Restarting, None);
        let snapshot = board.snapshot();
        let status = snapshot.participants.get("motion").expect("motion");
        assert_eq!(status.pid, None);
        assert_eq!(status.artifact_size_bytes, None);
    }

    fn spec(id: &str) -> ParticipantSpec {
        ParticipantSpec {
            id: id.to_string(),
            kind: ParticipantKind::Service,
            executable: PathBuf::from("/bin/echo"),
            args: Vec::new(),
            cwd: None,
            env: Vec::new(),
            shutdown_grace: Duration::from_millis(10),
            process_group: false,
            note: None,
            bus_participant: true,
        }
    }

    fn sleep_spec(id: &str) -> ParticipantSpec {
        ParticipantSpec {
            id: id.to_string(),
            kind: ParticipantKind::Service,
            executable: PathBuf::from("/bin/sh"),
            args: vec!["-c".to_string(), "sleep 30".to_string()],
            cwd: None,
            env: Vec::new(),
            shutdown_grace: Duration::from_millis(50),
            process_group: false,
            note: None,
            bus_participant: true,
        }
    }

    #[tokio::test]
    async fn closed_action_receiver_is_consumed_once_then_stays_pending() {
        let (action_tx, action_rx) = mpsc::channel(1);
        drop(action_tx);
        let mut action_rx = Some(action_rx);

        assert!(recv_action(&mut action_rx).await.is_none());
        assert!(action_rx.is_none());

        // A closed receiver is terminal, not a stream of immediate `None`s;
        // otherwise it would win every supervisor `select!` pass and spin.
        assert!(
            tokio::time::timeout(Duration::from_millis(20), recv_action(&mut action_rx))
                .await
                .is_err()
        );
    }

    #[test]
    fn router_ownership_distinguishes_external_from_managed() {
        assert_eq!(router_ownership(true), RouterOwnership::External);
        assert_eq!(router_ownership(false), RouterOwnership::Managed);
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
    async fn requested_webots_sigterm_exit_is_stopped_not_failed() -> Result<()> {
        let mut webots = sleep_spec("webots");
        webots.args = vec![
            "-c".to_string(),
            "trap 'exit 0' TERM; while :; do sleep 1; done".to_string(),
        ];
        webots.process_group = true;

        let board = BoardBackend::new();
        board.upsert(ParticipantStatus::new(
            "webots",
            ParticipantKind::Tool,
            ParticipantState::Starting,
        ));
        let participant = RunningParticipant::spawn(webots, &board).await?;
        tokio::time::sleep(Duration::from_millis(50)).await;
        let mut running = vec![participant];

        request_participant_stop(
            &mut running,
            &board,
            RequestedStop::new("webots", Duration::from_secs(1)),
        )
        .await;

        let snapshot = board.snapshot();
        let status = snapshot.participants.get("webots").expect("webots");
        assert_eq!(status.state, ParticipantState::Stopped);
        assert!(snapshot.failed_participants().is_empty());
        assert!(!running[0].is_active());
        Ok(())
    }

    #[tokio::test]
    async fn webots_crash_reaped_during_requested_stop_is_failed() -> Result<()> {
        let mut webots = sleep_spec("webots");
        webots.args = vec!["-c".to_string(), "exit 7".to_string()];
        webots.process_group = true;

        let board = BoardBackend::new();
        board.upsert(ParticipantStatus::new(
            "webots",
            ParticipantKind::Tool,
            ParticipantState::Starting,
        ));
        let participant = RunningParticipant::spawn(webots, &board).await?;
        tokio::time::sleep(Duration::from_millis(50)).await;
        let mut running = vec![participant];

        request_participant_stop(
            &mut running,
            &board,
            RequestedStop::new("webots", Duration::from_secs(1)),
        )
        .await;

        let snapshot = board.snapshot();
        let status = snapshot.participants.get("webots").expect("webots");
        assert_eq!(status.state, ParticipantState::Failed);
        let outcome = SupervisorOutcome {
            failed_participants: snapshot.failed_participants(),
        };
        assert!(!outcome.graph_healthy());
        assert_eq!(outcome.failed_participants, vec!["webots"]);
        assert!(!running[0].is_active());
        Ok(())
    }

    #[tokio::test]
    async fn webots_crash_already_waiting_to_restart_is_failed_at_stop() -> Result<()> {
        let mut webots = sleep_spec("webots");
        webots.args = vec!["-c".to_string(), "exit 7".to_string()];
        webots.process_group = true;

        let board = BoardBackend::new();
        board.upsert(ParticipantStatus::new(
            "webots",
            ParticipantKind::Tool,
            ParticipantState::Starting,
        ));
        let mut participant = RunningParticipant::spawn(webots, &board).await?;
        tokio::time::sleep(Duration::from_millis(50)).await;
        participant
            .poll(
                &board,
                &RestartPolicy {
                    restart_delay: Duration::from_secs(30),
                    ..RestartPolicy::default()
                },
            )
            .await?;
        assert!(participant.child.is_none());
        assert!(participant.restart_at.is_some());
        let mut running = vec![participant];

        request_participant_stop(
            &mut running,
            &board,
            RequestedStop::new("webots", Duration::from_secs(1)),
        )
        .await;

        let snapshot = board.snapshot();
        let status = snapshot.participants.get("webots").expect("webots");
        assert_eq!(status.state, ParticipantState::Failed);
        assert_eq!(snapshot.failed_participants(), vec!["webots"]);
        assert!(running[0].restart_at.is_none());
        assert!(!running[0].is_active());
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn requested_webots_stop_uses_sigkill_only_after_term_grace() -> Result<()> {
        let mut webots = sleep_spec("webots");
        webots.args = vec!["-c".to_string(), "trap '' TERM; sleep 30".to_string()];
        webots.process_group = true;
        let board = BoardBackend::new();
        board.upsert(ParticipantStatus::new(
            "webots",
            ParticipantKind::Tool,
            ParticipantState::Starting,
        ));
        let participant = RunningParticipant::spawn(webots, &board).await?;
        let pid = participant
            .child
            .as_ref()
            .and_then(Child::id)
            .context("test child has no pid")?;
        tokio::time::sleep(Duration::from_millis(50)).await;
        let mut running = vec![participant];

        request_participant_stop(
            &mut running,
            &board,
            RequestedStop::new("webots", Duration::from_millis(20)),
        )
        .await;

        assert!(!process_group_alive(pid)?);
        let snapshot = board.snapshot();
        let status = snapshot.participants.get("webots").expect("webots");
        assert_eq!(status.state, ParticipantState::Failed);
        assert!(
            status
                .note
                .as_deref()
                .is_some_and(|note| note.contains("SIGKILL fallback"))
        );
        Ok(())
    }

    #[tokio::test]
    async fn watch_swap_does_not_consume_restart_budget() -> Result<()> {
        let board = BoardBackend::new();
        board.upsert(ParticipantStatus::new(
            "mission",
            ParticipantKind::Service,
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
        // OBSERVED readiness: swap lands back at `Starting` (this fixture
        // never heartbeats), but the swap note is still attached immediately.
        assert_eq!(status.state, ParticipantState::Starting);
        assert_eq!(status.note.as_deref(), Some("ok 0.1s, restarted"));

        participant.stop_current(&board).await?;
        Ok(())
    }

    #[tokio::test]
    async fn restart_spawn_failure_is_participant_status_not_supervisor_error() -> Result<()> {
        let board = BoardBackend::new();
        board.upsert(ParticipantStatus::new(
            "mission",
            ParticipantKind::Service,
            ParticipantState::Starting,
        ));
        let mut participant = RunningParticipant::spawn(sleep_spec("mission"), &board).await?;
        participant.stop_current(&board).await?;
        participant.spec.executable = PathBuf::from("/definitely/missing/phoxal-participant");
        participant.restart_at = Some(Instant::now());

        participant.poll(&board, &RestartPolicy::default()).await?;

        assert!(participant.failed);
        let snapshot = board.snapshot();
        assert_eq!(
            snapshot.participants["mission"].state,
            ParticipantState::Failed
        );
        assert!(
            snapshot.participants["mission"]
                .note
                .as_deref()
                .is_some_and(|note| note.contains("restart spawn failed"))
        );
        Ok(())
    }

    #[tokio::test]
    async fn swap_spawn_failure_is_participant_status_not_supervisor_error() -> Result<()> {
        let board = BoardBackend::new();
        board.upsert(ParticipantStatus::new(
            "mission",
            ParticipantKind::Service,
            ParticipantState::Starting,
        ));
        let mut participant = RunningParticipant::spawn(sleep_spec("mission"), &board).await?;
        let mut missing = sleep_spec("mission");
        missing.executable = PathBuf::from("/definitely/missing/phoxal-participant");

        participant
            .swap(missing, &board, "watch rebuild completed".to_string())
            .await?;

        assert!(participant.failed);
        let snapshot = board.snapshot();
        assert_eq!(
            snapshot.participants["mission"].state,
            ParticipantState::Failed
        );
        assert!(
            snapshot.participants["mission"]
                .note
                .as_deref()
                .is_some_and(|note| note.contains("swap spawn failed"))
        );
        Ok(())
    }

    #[tokio::test]
    async fn all_children_may_fail_without_ending_the_session() -> Result<()> {
        let board = BoardBackend::new();
        board.upsert(ParticipantStatus::new(
            "flap",
            ParticipantKind::Service,
            ParticipantState::Starting,
        ));
        let flappy = ParticipantSpec {
            id: "flap".to_string(),
            kind: ParticipantKind::Service,
            executable: PathBuf::from("/bin/sh"),
            args: vec!["-c".to_string(), "exit 7".to_string()],
            cwd: None,
            env: Vec::new(),
            shutdown_grace: Duration::from_millis(10),
            process_group: false,
            note: None,
            bus_participant: true,
        };
        let token = tokio_util::sync::CancellationToken::new();
        let supervise = tokio::spawn(supervise_until_shutdown(
            vec![SupervisionStage::new(
                "stage",
                vec![flappy],
                WaitBudget::Bounded(Duration::from_secs(5)),
            )],
            board.clone(),
            SupervisorOptions {
                restart_policy: RestartPolicy {
                    restart_delay: Duration::from_millis(1),
                    start_limit_interval: Duration::from_secs(60),
                    start_limit_burst: 1,
                },
                token: token.clone(),
                ..SupervisorOptions::default()
            },
        ));

        tokio::time::timeout(Duration::from_secs(5), async {
            while board.snapshot().participants["flap"].state != ParticipantState::Failed {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the failed child must remain visible on the board");
        assert!(
            board.snapshot().participants["flap"]
                .note
                .as_deref()
                .is_some_and(|note| note.contains("StartLimitBurst")),
            "restart exhaustion must explain the terminal participant status"
        );
        tokio::time::sleep(Duration::from_millis(650)).await;
        assert!(
            !supervise.is_finished(),
            "even an all-failed graph must wait for the user's stop action"
        );

        token.cancel();
        let outcome = tokio::time::timeout(Duration::from_secs(3), supervise)
            .await
            .expect("user stop must tear down promptly")
            .expect("supervisor task panicked")?;
        assert_eq!(outcome.failed_participants, vec!["flap"]);
        Ok(())
    }

    #[tokio::test]
    async fn participant_failure_stays_visible_until_user_stop() -> Result<()> {
        let board = BoardBackend::new();
        board.upsert(ParticipantStatus::new(
            "webots",
            ParticipantKind::Tool,
            ParticipantState::Starting,
        ));
        board.upsert(ParticipantStatus::new(
            "simulator-webots-controller-robot",
            ParticipantKind::Service,
            ParticipantState::Ready,
        ));

        let mut webots = sleep_spec("webots");
        webots.kind = ParticipantKind::Tool;
        webots.args = vec![
            "-c".to_string(),
            "trap 'exit 0' TERM; while :; do sleep 1; done".to_string(),
        ];
        webots.process_group = true;
        webots.bus_participant = false;

        let token = tokio_util::sync::CancellationToken::new();
        let supervise = tokio::spawn(supervise_until_shutdown(
            vec![SupervisionStage::new(
                "stage",
                vec![webots],
                WaitBudget::Bounded(Duration::from_secs(5)),
            )],
            board.clone(),
            SupervisorOptions {
                requested_stop: Some(RequestedStop::new("webots", Duration::from_secs(1))),
                token: token.clone(),
                ..SupervisorOptions::default()
            },
        ));

        tokio::time::sleep(Duration::from_millis(75)).await;
        assert!(
            !supervise.is_finished(),
            "the session must remain up until the user stops it"
        );
        board.set_state(
            "simulator-webots-controller-robot",
            ParticipantState::Failed,
            Some("controller reported terminal failure".to_string()),
        );

        tokio::time::sleep(Duration::from_millis(650)).await;
        assert!(
            !supervise.is_finished(),
            "a failed participant must not terminate the session"
        );

        token.cancel();
        let outcome = tokio::time::timeout(Duration::from_secs(3), supervise)
            .await
            .expect("user stop must tear down promptly")
            .expect("supervisor task panicked")?;
        assert!(!outcome.graph_healthy());
        assert_eq!(
            outcome.failed_participants,
            vec!["simulator-webots-controller-robot"]
        );
        assert_eq!(
            board.snapshot().participants["webots"].state,
            ParticipantState::Stopped
        );
        Ok(())
    }

    #[tokio::test]
    async fn local_log_capture_works_without_bus() -> Result<()> {
        let board = BoardBackend::new();
        board.upsert(ParticipantStatus::new(
            "logger",
            ParticipantKind::Service,
            ParticipantState::Starting,
        ));
        let specs = vec![ParticipantSpec {
            id: "logger".to_string(),
            kind: ParticipantKind::Service,
            executable: PathBuf::from("/bin/sh"),
            args: vec!["-c".to_string(), "echo local-line; exit 2".to_string()],
            cwd: None,
            env: Vec::new(),
            shutdown_grace: Duration::from_millis(10),
            process_group: false,
            note: None,
            bus_participant: true,
        }];
        let token = tokio_util::sync::CancellationToken::new();
        let supervise = tokio::spawn(supervise_until_shutdown(
            vec![SupervisionStage::new(
                "stage",
                specs,
                WaitBudget::Bounded(Duration::from_secs(5)),
            )],
            board.clone(),
            SupervisorOptions {
                restart_policy: RestartPolicy {
                    restart_delay: Duration::from_millis(1),
                    start_limit_interval: Duration::from_secs(1),
                    start_limit_burst: 1,
                },
                action_rx: None,
                requested_stop: None,
                token: token.clone(),
                events: None,
                emits_running_on_startup_complete: false,
            },
        ));
        tokio::time::timeout(Duration::from_secs(5), async {
            while board.snapshot().participants["logger"].state != ParticipantState::Failed {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the logger exits before readiness and must fail its stage");

        let snapshot = board.snapshot();
        let status = snapshot.participants.get("logger").expect("logger status");
        assert!(
            status
                .last_log_lines
                .iter()
                .any(|line| line.contains("stdout: local-line")),
            "{status:?}"
        );
        token.cancel();
        let outcome = supervise.await.expect("supervisor task panicked")?;
        assert_eq!(outcome.failed_participants, vec!["logger"]);
        Ok(())
    }

    #[test]
    fn spawn_no_longer_marks_a_bus_participant_ready() {
        // The core observed-readiness invariant: `bus_participant: true` (the
        // default for every real phoxal participant, see the field docs) must
        // stay `Starting` through a successful spawn - `Ready` now comes only
        // from an observed heartbeat (`BoardBackend::record_heartbeat`).
        let board = BoardBackend::new();
        board.upsert(ParticipantStatus::new(
            "mission",
            ParticipantKind::Service,
            ParticipantState::Starting,
        ));
        board.record_heartbeat("mission", api::presence::Readiness::Initializing);
        let snapshot = board.snapshot();
        assert_eq!(
            snapshot.participants["mission"].state,
            ParticipantState::Starting
        );

        board.record_heartbeat("mission", api::presence::Readiness::Ready);
        let snapshot = board.snapshot();
        assert_eq!(
            snapshot.participants["mission"].state,
            ParticipantState::Ready
        );
    }

    #[test]
    fn stale_sweep_does_not_fail_a_participant_still_in_setup() {
        // Bug 1: the runner publishes one `Initializing` heartbeat before
        // `#[setup]` runs, then nothing until the post-setup `Ready` beacon.
        // A participant whose `#[setup]` legitimately runs long is therefore
        // heartbeat-silent, and must NOT be false-`Failed` by the 5s sweep -
        // that case belongs to the startup stage's own (longer) timeout.
        let board = BoardBackend::new();
        board.upsert(ParticipantStatus::new(
            "slow-setup",
            ParticipantKind::Service,
            ParticipantState::Starting,
        ));
        board.record_heartbeat("slow-setup", api::presence::Readiness::Initializing);
        // Back-date the recorded heartbeat well past the staleness threshold
        // without sleeping, matching this module's deterministic style.
        board
            .heartbeats
            .lock()
            .expect("heartbeat mutex poisoned")
            .insert(
                "slow-setup".to_string(),
                Instant::now() - Duration::from_secs(60),
            );

        board.mark_stale_heartbeats(Duration::from_secs(5));

        let snapshot = board.snapshot();
        assert_eq!(
            snapshot.participants["slow-setup"].state,
            ParticipantState::Starting,
            "a participant that never reached Ready must not be marked Failed by the sweep"
        );
    }

    #[test]
    fn stale_sweep_still_fails_a_participant_that_went_silent_after_ready() {
        // Detection must be preserved: once a participant has genuinely
        // reached `Ready`, going silent for longer than the threshold must
        // still mark it `Failed` when its process lifecycle belongs to the
        // CLI. Webots-owned participants use the recoverable policy tested
        // below.
        let board = BoardBackend::new();
        board.upsert(ParticipantStatus::new(
            "was-ready",
            ParticipantKind::Service,
            ParticipantState::Starting,
        ));
        board.record_heartbeat("was-ready", api::presence::Readiness::Ready);
        board
            .heartbeats
            .lock()
            .expect("heartbeat mutex poisoned")
            .insert(
                "was-ready".to_string(),
                Instant::now() - Duration::from_secs(60),
            );

        board.mark_stale_heartbeats(Duration::from_secs(5));

        let snapshot = board.snapshot();
        assert_eq!(
            snapshot.participants["was-ready"].state,
            ParticipantState::Failed,
            "a participant that reached Ready and then went silent must still be caught"
        );
    }

    #[test]
    fn webots_managed_presence_can_disappear_and_recover() {
        let id = "simulator-webots-controller-robot-v1";
        let board = BoardBackend::new();
        board.mark_presence_recoverable(id);
        board.upsert(ParticipantStatus::new(
            id,
            ParticipantKind::Simulator,
            ParticipantState::Starting,
        ));
        board.record_heartbeat(id, api::presence::Readiness::Ready);
        board
            .heartbeats
            .lock()
            .expect("heartbeat mutex poisoned")
            .insert(id.to_string(), Instant::now() - Duration::from_secs(60));

        board.mark_stale_heartbeats(Duration::from_secs(5));

        let snapshot = board.snapshot();
        assert_eq!(
            snapshot.participants[id].state,
            ParticipantState::Degraded,
            "losing a Webots-owned participant is observable, not terminal"
        );

        board.record_heartbeat(id, api::presence::Readiness::Ready);
        let snapshot = board.snapshot();
        assert_eq!(snapshot.participants[id].state, ParticipantState::Ready);
        assert_eq!(
            snapshot.participants[id].note.as_deref(),
            Some("Webots-managed participant observed again")
        );

        board.record_heartbeat(id, api::presence::Readiness::Failed);
        assert_eq!(
            board.snapshot().participants[id].state,
            ParticipantState::Failed
        );
        board.record_heartbeat(id, api::presence::Readiness::Ready);
        assert_eq!(
            board.snapshot().participants[id].state,
            ParticipantState::Ready,
            "an externally managed participant recreated after self-reporting Failed may recover"
        );
    }

    #[test]
    fn webots_managed_degraded_heartbeat_keeps_the_degraded_context() {
        let id = "simulator-webots-controller-robot-v1";
        let board = BoardBackend::new();
        board.mark_presence_recoverable(id);
        board.upsert(ParticipantStatus::new(
            id,
            ParticipantKind::Simulator,
            ParticipantState::Degraded,
        ));
        board.set_note(id, "Webots reports degraded operation");

        board.record_heartbeat(id, api::presence::Readiness::Degraded);

        let snapshot = board.snapshot();
        assert_eq!(snapshot.participants[id].state, ParticipantState::Degraded);
        assert_eq!(
            snapshot.participants[id].note.as_deref(),
            Some("Webots reports degraded operation"),
            "re-observation must not overwrite a self-reported degraded status with a recovery note"
        );
    }

    #[tokio::test]
    async fn respawn_clears_stale_pre_crash_heartbeat_and_ready_once() -> Result<()> {
        // Bug 2: after a crash+restart, `spawn_child` resets board STATE to
        // `Starting` but must also clear the `heartbeats`/`ready_once`
        // bookkeeping - otherwise the stale pre-crash timestamp survives and
        // can immediately re-`Fail` the freshly respawned process, and (per
        // Bug 1) it must re-earn `Ready` before the sweep applies to it again.
        let board = BoardBackend::new();
        board.upsert(ParticipantStatus::new(
            "mission",
            ParticipantKind::Service,
            ParticipantState::Starting,
        ));
        let mut participant = RunningParticipant::spawn(sleep_spec("mission"), &board).await?;
        // Simulate the pre-crash incarnation having reached Ready, then a long
        // silence (as if it crashed and nobody observed a heartbeat since).
        board.record_heartbeat("mission", api::presence::Readiness::Ready);
        board
            .heartbeats
            .lock()
            .expect("heartbeat mutex poisoned")
            .insert(
                "mission".to_string(),
                Instant::now() - Duration::from_secs(60),
            );

        // Respawn (as `poll` does after a crash, and `swap`/`resume` do too).
        participant.spawn_child(&board).await?;

        assert!(
            !board
                .heartbeats
                .lock()
                .expect("heartbeat mutex poisoned")
                .contains_key("mission"),
            "respawn must clear the stale pre-crash heartbeat timestamp"
        );
        assert!(
            !board
                .ready_once
                .lock()
                .expect("ready-once mutex poisoned")
                .contains("mission"),
            "respawn must clear the has-been-Ready bit so Ready must be earned again"
        );

        // And the stale timestamp being gone means the sweep must not
        // immediately re-fail the fresh incarnation.
        board.mark_stale_heartbeats(Duration::from_secs(5));
        let snapshot = board.snapshot();
        assert_eq!(
            snapshot.participants["mission"].state,
            ParticipantState::Starting,
            "a freshly respawned participant must not be immediately re-failed"
        );

        participant.stop_current(&board).await?;
        Ok(())
    }

    #[test]
    fn heartbeat_cannot_resurrect_a_terminal_participant() {
        let board = BoardBackend::new();
        board.upsert(ParticipantStatus::new(
            "mission",
            ParticipantKind::Service,
            ParticipantState::Failed,
        ));
        // A heartbeat that was in flight when the process independently died
        // must not undo the failure the process supervisor already recorded.
        board.record_heartbeat("mission", api::presence::Readiness::Ready);
        let snapshot = board.snapshot();
        assert_eq!(
            snapshot.participants["mission"].state,
            ParticipantState::Failed
        );
    }

    /// A simulation-managed participant (the Webots supervisor/controller: no
    /// `ParticipantSpec`, no supervised process, launched by Webots itself) that
    /// never heartbeats must both (a) make participant readiness fail with a
    /// clear, bounded-time error instead of hanging, and (b) be counted as
    /// failed afterward so `SupervisorOutcome::graph_healthy` reflects it even
    /// though no process crash was ever observed.
    #[tokio::test]
    async fn participant_wait_times_out_on_a_simulation_managed_participant_that_never_appears() {
        let board = BoardBackend::new();
        board.upsert(ParticipantStatus::new(
            "simulator-webots-supervisor",
            ParticipantKind::Tool,
            ParticipantState::Starting,
        ));
        board.upsert(ParticipantStatus::new(
            "simulator-webots-controller-robot",
            ParticipantKind::Tool,
            ParticipantState::Starting,
        ));
        // The supervisor checks in...
        board.record_heartbeat(
            "simulator-webots-supervisor",
            api::presence::Readiness::Ready,
        );
        // ...but the controller never does.

        let expected = vec![
            "simulator-webots-supervisor".to_string(),
            "simulator-webots-controller-robot".to_string(),
        ];
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            await_participants_ready(
                &board,
                &expected,
                WaitBudget::Bounded(Duration::from_millis(300)),
                Duration::from_millis(20),
            ),
        )
        .await
        .expect("participant wait must return within its own timeout, never hang");

        let error = result.expect_err("a controller that never appears must fail the barrier");
        assert!(
            error
                .to_string()
                .contains("simulator-webots-controller-robot"),
            "error should name the missing participant: {error}"
        );

        // Failure propagation: the wait's own board-marking side
        // effect is what makes the graph unhealthy, even though this
        // participant never had a supervised process to crash.
        let snapshot = board.snapshot();
        assert_eq!(
            snapshot.participants["simulator-webots-controller-robot"].state,
            ParticipantState::Failed
        );
        assert_eq!(
            snapshot.participants["simulator-webots-supervisor"].state,
            ParticipantState::Ready,
            "the participant that DID heartbeat must not be dragged down by the other's timeout"
        );
        let outcome = SupervisorOutcome {
            failed_participants: snapshot.failed_participants(),
        };
        assert!(!outcome.graph_healthy());
        assert_eq!(
            outcome.failed_participants,
            vec!["simulator-webots-controller-robot"]
        );
    }

    #[tokio::test]
    async fn participant_wait_fails_immediately_on_explicit_terminal_readiness_failure() {
        let board = BoardBackend::new();
        board.upsert(ParticipantStatus::new(
            "simulator-webots-controller-robot",
            ParticipantKind::Service,
            ParticipantState::Starting,
        ));
        board.record_heartbeat(
            "simulator-webots-controller-robot",
            api::presence::Readiness::Failed,
        );
        let expected = vec!["simulator-webots-controller-robot".to_string()];

        let error = tokio::time::timeout(
            Duration::from_millis(200),
            await_participants_ready(
                &board,
                &expected,
                WaitBudget::Bounded(Duration::from_secs(60)),
                Duration::from_millis(10),
            ),
        )
        .await
        .expect("explicit Failed readiness must bypass the startup timeout")
        .expect_err("explicit Failed readiness must fail the barrier");

        assert_eq!(
            error.to_string(),
            "stage ended unhealthy; failed participants: simulator-webots-controller-robot"
        );
    }

    #[tokio::test]
    async fn await_participants_ready_succeeds_once_everything_is_observed() {
        let board = BoardBackend::new();
        board.upsert(ParticipantStatus::new(
            "mission",
            ParticipantKind::Service,
            ParticipantState::Starting,
        ));
        board.record_heartbeat("mission", api::presence::Readiness::Ready);

        await_participants_ready(
            &board,
            &["mission".to_string()],
            WaitBudget::Bounded(Duration::from_secs(5)),
            Duration::from_millis(10),
        )
        .await
        .expect("everything is ready; must not error");
    }

    #[tokio::test]
    async fn await_participants_ready_times_out_and_marks_missing_failed() {
        let board = BoardBackend::new();
        board.upsert(ParticipantStatus::new(
            "mission",
            ParticipantKind::Service,
            ParticipantState::Starting,
        ));

        let error = tokio::time::timeout(
            Duration::from_secs(5),
            await_participants_ready(
                &board,
                &["mission".to_string()],
                WaitBudget::Bounded(Duration::from_millis(150)),
                Duration::from_millis(10),
            ),
        )
        .await
        .expect("must return within its own timeout, never hang")
        .expect_err("a participant that never appears must fail the wait");

        assert!(
            error.to_string().contains("mission"),
            "error should name the missing participant: {error}"
        );
        assert_eq!(
            board.snapshot().participants["mission"].state,
            ParticipantState::Failed
        );
    }

    #[tokio::test]
    async fn await_participants_ready_fails_immediately_on_explicit_terminal_failure() {
        let board = BoardBackend::new();
        board.upsert(ParticipantStatus::new(
            "mission",
            ParticipantKind::Service,
            ParticipantState::Starting,
        ));
        board.record_heartbeat("mission", api::presence::Readiness::Failed);

        let error = tokio::time::timeout(
            Duration::from_millis(200),
            await_participants_ready(
                &board,
                &["mission".to_string()],
                WaitBudget::Bounded(Duration::from_secs(60)),
                Duration::from_millis(10),
            ),
        )
        .await
        .expect("explicit Failed readiness must bypass the stage timeout")
        .expect_err("explicit Failed readiness must fail the wait");

        assert_eq!(
            error.to_string(),
            "stage ended unhealthy; failed participants: mission"
        );
    }

    /// The core staged-startup acceptance: nothing in a later stage spawns
    /// until the previous stage is OBSERVED ready (not merely spawned).
    /// "two" (`sleep_spec`, `bus_participant: true`) never gets a heartbeat
    /// from this test until it manually sends one, so its absence from the
    /// board proves stage two has not spawned yet.
    #[tokio::test]
    async fn staged_startup_gates_the_next_stage_on_observed_readiness() -> Result<()> {
        let board = BoardBackend::new();
        // Every real caller (`prepare_site_tools`/`prepare_robot_participants`)
        // upserts a `Starting` board entry BEFORE a spec ever reaches the
        // supervisor - `BoardBackend::append_log`'s missing-entry fallback
        // defaults to `Ready`, so a spec spawned with no pre-existing entry
        // would look observed-ready immediately, defeating the gate this
        // test proves. Mirror that real contract for "one" only - "two"
        // deliberately stays un-upserted, so its absence from the board is
        // this test's proof that stage two has not spawned yet.
        board.upsert(ParticipantStatus::new(
            "one",
            ParticipantKind::Service,
            ParticipantState::Starting,
        ));
        let stages = vec![
            SupervisionStage::new(
                "stage-one",
                vec![sleep_spec("one")],
                WaitBudget::Bounded(Duration::from_secs(5)),
            ),
            SupervisionStage::new(
                "stage-two",
                vec![sleep_spec("two")],
                WaitBudget::Bounded(Duration::from_secs(5)),
            ),
        ];
        let token = tokio_util::sync::CancellationToken::new();
        let supervise = tokio::spawn(supervise_until_shutdown(
            stages,
            board.clone(),
            SupervisorOptions {
                token: token.clone(),
                ..SupervisorOptions::default()
            },
        ));

        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(
            !board.snapshot().participants.contains_key("two"),
            "stage two must not spawn until stage one is observed ready"
        );

        board.record_heartbeat("one", api::presence::Readiness::Ready);
        tokio::time::timeout(Duration::from_secs(3), async {
            while !board.snapshot().participants.contains_key("two") {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("stage two must spawn once stage one is observed ready");

        token.cancel();
        tokio::time::timeout(Duration::from_secs(3), supervise)
            .await
            .expect("supervisor must exit promptly")
            .expect("supervisor task panicked")?;
        Ok(())
    }

    /// The router gate is independent of process/TCP readiness: downstream
    /// stays unspawned until the production bus probe reports success.
    #[tokio::test]
    async fn router_stage_waits_for_the_probe_before_downstream_spawn() -> Result<()> {
        let board = BoardBackend::new();
        let router_ready = Arc::new(AtomicBool::new(false));
        let mut downstream = sleep_spec("downstream");
        downstream.bus_participant = false;
        let stages = vec![
            SupervisionStage::new(
                "connecting to router",
                Vec::new(),
                WaitBudget::Bounded(Duration::from_secs(5)),
            )
            .with_manual_router_readiness(Arc::clone(&router_ready)),
            SupervisionStage::new(
                "starting downstream",
                vec![downstream],
                WaitBudget::Bounded(Duration::from_secs(1)),
            ),
        ];
        let token = tokio_util::sync::CancellationToken::new();
        let supervise = tokio::spawn(supervise_until_shutdown(
            stages,
            board.clone(),
            SupervisorOptions {
                token: token.clone(),
                ..SupervisorOptions::default()
            },
        ));

        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(
            !board.snapshot().participants.contains_key("downstream"),
            "downstream must remain unspawned while the router probe is pending"
        );
        router_ready.store(true, Ordering::Release);
        tokio::time::timeout(Duration::from_secs(3), async {
            while !board.snapshot().participants.contains_key("downstream") {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("downstream must spawn after router probe success");

        token.cancel();
        tokio::time::timeout(Duration::from_secs(3), supervise)
            .await
            .expect("supervisor must exit promptly")
            .expect("supervisor task panicked")?;
        Ok(())
    }

    /// A crashing member of the CURRENT (still-waiting) stage must keep
    /// restarting while that stage's own readiness wait is pending - the
    /// wait runs as one branch of the same `select!` as poll/restart, not
    /// inline before it, so it can never freeze supervision.
    #[tokio::test]
    async fn restart_still_happens_while_a_stage_readiness_wait_is_pending() -> Result<()> {
        let board = BoardBackend::new();
        board.upsert(ParticipantStatus::new(
            "flap",
            ParticipantKind::Service,
            ParticipantState::Starting,
        ));
        let flappy = ParticipantSpec {
            id: "flap".to_string(),
            kind: ParticipantKind::Service,
            executable: PathBuf::from("/bin/sh"),
            args: vec!["-c".to_string(), "exit 7".to_string()],
            cwd: None,
            env: Vec::new(),
            shutdown_grace: Duration::from_millis(10),
            process_group: false,
            note: None,
            // Never heartbeats, so this stage's own wait never completes -
            // the whole observation window below runs with `pending_stage`
            // `Some`, proving restart still happens during that wait.
            bus_participant: true,
        };
        let stages = vec![SupervisionStage::new(
            "stage-one",
            vec![flappy],
            WaitBudget::Bounded(Duration::from_secs(30)),
        )];
        let token = tokio_util::sync::CancellationToken::new();
        let supervise = tokio::spawn(supervise_until_shutdown(
            stages,
            board.clone(),
            SupervisorOptions {
                restart_policy: RestartPolicy {
                    restart_delay: Duration::from_millis(20),
                    start_limit_interval: Duration::from_secs(60),
                    start_limit_burst: 1000,
                },
                token: token.clone(),
                ..SupervisorOptions::default()
            },
        ));

        // Poll/restart only happen on the supervisor's 500ms ticker branch,
        // so the observation window must span several ticks to prove
        // restarts keep accumulating - too short a window would just prove
        // "no crash happened yet", not "restart runs concurrently with the
        // wait".
        tokio::time::sleep(Duration::from_millis(1700)).await;
        let restart_count = board.snapshot().participants["flap"].restart_count;
        assert!(
            restart_count >= 2,
            "expected multiple restarts while the stage-one readiness wait was still pending, got {restart_count}"
        );
        // "flap" is mid crash-loop, so its state at this exact instant is
        // either `Starting` (just respawned) or `Restarting` (waiting out
        // `restart_delay`) - never `Ready`, since it never heartbeats.
        assert!(
            matches!(
                board.snapshot().participants["flap"].state,
                ParticipantState::Starting | ParticipantState::Restarting
            ),
            "flap never heartbeats, so stage one's own wait must still be pending: {:?}",
            board.snapshot().participants["flap"].state
        );

        token.cancel();
        tokio::time::timeout(Duration::from_secs(3), supervise)
            .await
            .expect("supervisor must exit promptly")
            .expect("supervisor task panicked")?;
        Ok(())
    }

    /// Cancelling the session token WHILE a stage readiness wait is still
    /// pending must still
    /// break the loop and tear down promptly, proving the wait branch never
    /// starves the other `select!` branches.
    #[tokio::test]
    async fn cancellation_during_a_stage_wait_still_tears_down_promptly() -> Result<()> {
        let board = BoardBackend::new();
        board.upsert(ParticipantStatus::new(
            "one",
            ParticipantKind::Service,
            ParticipantState::Starting,
        ));
        let stages = vec![SupervisionStage::new(
            "stage-one",
            vec![sleep_spec("one")],
            WaitBudget::Bounded(Duration::from_secs(30)),
        )];
        let token = tokio_util::sync::CancellationToken::new();
        let supervise = tokio::spawn(supervise_until_shutdown(
            stages,
            board.clone(),
            SupervisorOptions {
                token: token.clone(),
                ..SupervisorOptions::default()
            },
        ));

        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            board.snapshot().participants["one"].state,
            ParticipantState::Starting,
            "stage one must still be waiting on its own readiness"
        );

        token.cancel();
        // The proof is promptness itself: `supervise_until_shutdown` must
        // return well inside the timeout even though "one"'s own stage wait
        // was still pending when the cancel arrived - a plain (non-requested)
        // teardown via `shutdown_all` stops the child but does not relabel
        // its board state (that relabeling is `RequestedStop`-specific, see
        // `request_participant_stop`).
        tokio::time::timeout(Duration::from_secs(3), supervise)
            .await
            .expect("supervisor must exit promptly even mid-stage-wait")
            .expect("supervisor task panicked")?;
        Ok(())
    }

    /// A stage that never reaches readiness marks its stalled participants but
    /// still leaves the session running until the user stops it.
    #[tokio::test]
    async fn stalled_stage_times_out_and_marks_missing_participants_failed() -> Result<()> {
        let board = BoardBackend::new();
        board.upsert(ParticipantStatus::new(
            "one",
            ParticipantKind::Service,
            ParticipantState::Starting,
        ));
        let stages = vec![SupervisionStage::new(
            "stage-one",
            vec![sleep_spec("one")],
            WaitBudget::Bounded(Duration::from_millis(150)),
        )];
        let token = tokio_util::sync::CancellationToken::new();
        let supervise = tokio::spawn(supervise_until_shutdown(
            stages,
            board.clone(),
            SupervisorOptions {
                token: token.clone(),
                ..SupervisorOptions::default()
            },
        ));
        tokio::time::timeout(Duration::from_secs(5), async {
            while board.snapshot().participants["one"].state != ParticipantState::Failed {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("a stalled stage must be marked within its own timeout");

        assert!(
            !supervise.is_finished(),
            "a stalled stage is status, not session termination authority"
        );
        assert_eq!(board.snapshot().failed_participants(), vec!["one"]);
        token.cancel();
        let outcome = supervise.await.expect("supervisor task panicked")?;
        assert_eq!(outcome.failed_participants, vec!["one"]);
        Ok(())
    }

    #[tokio::test]
    async fn failed_stage_does_not_block_later_stage_or_end_session() -> Result<()> {
        let board = BoardBackend::new();
        for id in ["broken", "later"] {
            board.upsert(ParticipantStatus::new(
                id,
                ParticipantKind::Service,
                ParticipantState::Starting,
            ));
        }
        let mut broken = sleep_spec("broken");
        broken.executable = PathBuf::from("/definitely/missing/phoxal-participant");
        let mut later = sleep_spec("later");
        later.bus_participant = false;
        let token = tokio_util::sync::CancellationToken::new();
        let supervise = tokio::spawn(supervise_until_shutdown(
            vec![
                SupervisionStage::new(
                    "broken-stage",
                    vec![broken],
                    WaitBudget::Bounded(Duration::from_secs(2)),
                ),
                SupervisionStage::new(
                    "later-stage",
                    vec![later],
                    WaitBudget::Bounded(Duration::from_secs(2)),
                ),
            ],
            board.clone(),
            SupervisorOptions {
                token: token.clone(),
                ..SupervisorOptions::default()
            },
        ));

        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let snapshot = board.snapshot();
                if snapshot.participants["broken"].state == ParticipantState::Failed
                    && snapshot.participants["later"].state == ParticipantState::Ready
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the later stage must start after the earlier stage fails");
        assert!(
            !supervise.is_finished(),
            "stage failure is visible status and the session still waits for user stop"
        );

        token.cancel();
        let outcome = supervise.await.expect("supervisor task panicked")?;
        assert_eq!(outcome.failed_participants, vec!["broken"]);
        Ok(())
    }
}
