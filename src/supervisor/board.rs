//! Concurrent session-board state derived from process and bus observations.

use super::{
    BoardSnapshot, LogScope, LogSeverity, LogSource, ParticipantLaunchCommand, ParticipantState,
    ParticipantStatus, RoutedLogLine, RoutedLogUpdate, bounded_chars, bounded_log_text,
};
use phoxal_cli_core::session::ParticipantKind;
use phoxal_cli_core::session::{
    BoundedString, ParticipantInstanceKey, ProcessDescriptor, ProcessEntry, ProcessFailure,
    ProcessFailureKind, ProcessKey, ProcessState, ProjectLifecycle, RobotKey, StartupRequirement,
    SupervisorSnapshotV0,
};
use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;
use tokio::sync::{mpsc, watch};

#[derive(Debug, Clone)]
pub struct BoardBackend {
    inner: Arc<Mutex<BoardSnapshot>>,
    state: Arc<Mutex<SupervisorStateActor>>,
    snapshot_tx: watch::Sender<SupervisorSnapshotV0>,
    /// Current robot-scoped Liveliness membership. This mirrors Zenoh's
    /// binary present/not-present observation without timestamps, leases, or
    /// inferred instances. Replacement processes may overlap under one stable
    /// key, producing continuous presence rather than another `Alive` event.
    presence: Arc<Mutex<PresenceState>>,
    recovery_epoch: Arc<AtomicU64>,
    recovery_epoch_tx: watch::Sender<u64>,
    /// Optional live sink for [`RoutedLogLine`]s - set by
    /// the resident supervisor once its
    /// `TuiDisplay` renderer exists, so it can maintain its own bounded
    /// per-runtime scrollback (Part 3) without polling the board's own
    /// 8-line history. `None` outside a TUI session (Plain/Json output, or
    /// any test) - [`Self::route_log`] simply skips the broadcast in that
    /// case. Bounded: the consuming `TuiDisplay::redraw` already keeps its
    /// own bounded ring per runtime (`stores::log_store::LogStore`), so a
    /// full channel here just means a redraw is overdue - `route_log` drops
    /// the newest line rather than blocking the log-subscriber task that
    /// calls it.
    log_sink: Arc<Mutex<Option<mpsc::Sender<RoutedLogUpdate>>>>,
    /// First-seen unknown bus ids already disclosed to the operator. The set
    /// is deliberately small: an untrusted publisher cannot turn diagnostics
    /// about rejected ids into another unbounded allocation vector.
    unknown_bus_ids: Arc<Mutex<BTreeSet<String>>>,
}

#[derive(Debug, Default)]
struct PresenceState {
    enabled: bool,
    instances: BTreeSet<ParticipantInstanceKey>,
}

#[derive(Debug)]
struct SupervisorStateActor {
    snapshot: SupervisorSnapshotV0,
    used_incarnations: BTreeSet<u64>,
}

impl SupervisorStateActor {
    fn publish(&mut self, sender: &watch::Sender<SupervisorSnapshotV0>) {
        self.snapshot.revision = self.snapshot.revision.saturating_add(1);
        sender.send_replace(self.snapshot.clone());
    }
}

impl Default for BoardBackend {
    fn default() -> Self {
        let (recovery_epoch_tx, _) = watch::channel(0);
        let snapshot = SupervisorSnapshotV0 {
            supervisor_generation: random_nonzero_u64(),
            ..SupervisorSnapshotV0::default()
        };
        let (snapshot_tx, _) = watch::channel(snapshot.clone());
        Self {
            inner: Arc::default(),
            state: Arc::new(Mutex::new(SupervisorStateActor {
                snapshot,
                used_incarnations: BTreeSet::new(),
            })),
            snapshot_tx,
            presence: Arc::new(Mutex::new(PresenceState {
                enabled: true,
                instances: BTreeSet::new(),
            })),
            recovery_epoch: Arc::default(),
            recovery_epoch_tx,
            log_sink: Arc::default(),
            unknown_bus_ids: Arc::default(),
        }
    }
}

impl BoardBackend {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Replace the disposable client-side projection with authoritative
    /// resident state. Local Zenoh-derived fields are retained for rows that
    /// still exist; they are never copied into the supervisor snapshot.
    pub fn replace_from_supervisor(&self, snapshot: SupervisorSnapshotV0) {
        let mut board = self.inner.lock().expect("board mutex poisoned");
        let previous = std::mem::take(&mut board.participants);
        for (key, entry) in &snapshot.processes {
            let id = key.to_string();
            let mut status = previous.get(&id).cloned().unwrap_or_else(|| {
                ParticipantStatus::new(
                    id.clone(),
                    entry.descriptor.kind,
                    ParticipantState::Starting,
                )
            });
            status.kind = entry.descriptor.kind;
            status.state = participant_state(entry.status.actual);
            status.pid = entry.status.pid;
            status.restart_count = entry.status.restart_count_in_generation;
            status.note = entry
                .status
                .last_failure
                .as_ref()
                .map(|failure| failure.detail.as_str().to_string())
                .or(status.note);
            board.participants.insert(id, status);
        }
        drop(board);
        let mut actor = self.state.lock().expect("supervisor state mutex poisoned");
        actor.snapshot = snapshot;
        self.snapshot_tx.send_replace(actor.snapshot.clone());
    }

    /// Record Zenoh Liveliness for a planned participant.
    ///
    /// Launch planning must register every expected row before the observer
    /// starts. Presence for an unknown id is dropped rather than deferred, so
    /// untrusted bus keys cannot grow supervisor state.
    ///
    /// Appearance of the exact expected incarnation is the sole transition
    /// to `Ready`. Disappearance is observational and never mutates process
    /// lifecycle or commands a restart. Direct process lifecycle and startup
    /// timeouts retain that authority. Observations cannot resurrect terminal
    /// process state.
    pub fn record_instance_presence(&self, instance: ParticipantInstanceKey, present: bool) {
        let robot_key = ProcessKey::robot(instance.robot.clone(), instance.participant.clone());
        let project_key = ProcessKey::project(&instance.participant);
        let process_key = {
            let actor = self.state.lock().expect("supervisor state mutex poisoned");
            if actor.snapshot.processes.contains_key(&robot_key) {
                Some(robot_key.clone())
            } else if actor.snapshot.processes.contains_key(&project_key) {
                Some(project_key)
            } else {
                None
            }
        };
        let Some(process_key) = process_key else {
            let display_id = robot_key.to_string();
            self.disclose_unknown_bus_id(&display_id, "liveliness");
            return;
        };
        let display_id = process_key.to_string();
        let mut presence = self.presence.lock().expect("presence mutex poisoned");
        if !presence.enabled {
            return;
        }
        if present {
            presence.instances.insert(instance.clone());
        } else {
            presence.instances.remove(&instance);
        }
        let stable_present = presence.instances.iter().any(|candidate| {
            candidate.robot == instance.robot && candidate.participant == instance.participant
        });
        drop(presence);
        if let Some(status) = self
            .inner
            .lock()
            .expect("board mutex poisoned")
            .participants
            .get_mut(&display_id)
        {
            status.present = Some(stable_present);
        }

        // Exact Liveliness is a one-way startup proof. Once the exact minted
        // incarnation is ready, later token loss remains observational and
        // cannot mutate process lifecycle or invoke failure policy.
        if present {
            let exact = self
                .state
                .lock()
                .expect("supervisor state mutex poisoned")
                .snapshot
                .processes
                .get(&process_key)
                .is_some_and(|entry| {
                    entry.status.incarnation == Some(instance.incarnation)
                        && entry.status.actual == ProcessState::Starting
                });
            if exact {
                self.set_state(&process_key, ParticipantState::Ready, None);
            }
        }
    }

    #[cfg(test)]
    pub fn record_presence(&self, id: &str, present: bool) {
        if !self
            .presence
            .lock()
            .expect("presence mutex poisoned")
            .enabled
        {
            return;
        }
        let key = ProcessKey::from(id);
        if let Some(status) = self
            .inner
            .lock()
            .expect("board mutex poisoned")
            .participants
            .get_mut(&key.to_string())
        {
            status.present = Some(present);
        }
        if present && self.process_state(&key) == Some(ProcessState::Starting) {
            self.set_state(key, ParticipantState::Ready, None);
        }
    }

    #[must_use]
    pub fn is_exact_present(&self, instance: &ParticipantInstanceKey) -> bool {
        self.presence
            .lock()
            .expect("presence mutex poisoned")
            .instances
            .contains(instance)
    }

    #[cfg(test)]
    pub fn is_present(&self, id: &str) -> bool {
        self.inner
            .lock()
            .expect("board mutex poisoned")
            .participants
            .get(id)
            .and_then(|status| status.present)
            .unwrap_or(false)
    }

    /// Fence observations from the dead router and reset every graph-owned row
    /// before a replacement router or child is started. Wait-only readiness
    /// rows retain their authored notes because they have no spawned spec.
    pub(crate) fn begin_recovery_epoch(
        &self,
        spawned: &[(ProcessKey, Option<String>)],
        wait_only: &[ProcessKey],
    ) -> u64 {
        let mut snapshot = self.inner.lock().expect("board mutex poisoned");
        let mut presence = self.presence.lock().expect("presence mutex poisoned");
        presence.enabled = false;
        presence.instances.clear();
        for (key, note) in spawned {
            if let Some(status) = snapshot.participants.get_mut(&key.to_string()) {
                status.state = ParticipantState::Starting;
                status.note.clone_from(note);
                status.pid = None;
                status.artifact_size_bytes = None;
                status.restart_count = 0;
                status.present = None;
            }
        }
        for key in wait_only {
            if let Some(status) = snapshot.participants.get_mut(&key.to_string()) {
                status.state = ParticipantState::Starting;
                status.pid = None;
                status.artifact_size_bytes = None;
                status.restart_count = 0;
                status.present = None;
            }
        }
        drop(snapshot);
        let mut actor = self.state.lock().expect("supervisor state mutex poisoned");
        for key in spawned.iter().map(|(key, _)| key).chain(wait_only) {
            if let Some(entry) = actor.snapshot.processes.get_mut(key) {
                entry.status.actual = ProcessState::Starting;
                entry.status.pid = None;
                entry.status.restart_count_in_generation = 0;
            }
        }
        actor.snapshot.graph_generation = actor.snapshot.graph_generation.saturating_add(1);
        actor.snapshot.lifecycle = ProjectLifecycle::Starting;
        actor.snapshot.startup.active_phase = None;
        actor.snapshot.startup.completed_phases.clear();
        actor.publish(&self.snapshot_tx);
        let epoch = self.recovery_epoch.fetch_add(1, Ordering::SeqCst) + 1;
        self.recovery_epoch_tx.send_replace(epoch);
        epoch
    }

    pub(crate) fn enable_presence_for_recovery(&self) {
        self.presence
            .lock()
            .expect("presence mutex poisoned")
            .enabled = true;
    }

    /// Subscribe to full-graph recovery resets. Consumers recreate transport
    /// handles after every epoch so stale router sessions cannot strand a
    /// snapshot/follow reconciler waiting on the dead graph.
    pub(crate) fn recovery_epoch_receiver(&self) -> watch::Receiver<u64> {
        self.recovery_epoch_tx.subscribe()
    }

    #[cfg(test)]
    pub(crate) fn recovery_epoch(&self) -> u64 {
        self.recovery_epoch.load(Ordering::SeqCst)
    }

    pub fn upsert_process(
        &self,
        key: ProcessKey,
        mut status: ParticipantStatus,
        startup_requirement: StartupRequirement,
    ) {
        status.id = key.to_string();
        self.inner
            .lock()
            .expect("board mutex poisoned")
            .participants
            .insert(status.id.clone(), status.clone());
        let mut actor = self.state.lock().expect("supervisor state mutex poisoned");
        actor.snapshot.processes.insert(
            key.clone(),
            ProcessEntry {
                descriptor: ProcessDescriptor {
                    key,
                    kind: status.kind,
                    artifact: status.id,
                    owner: "phoxal-cli".to_string(),
                    startup_requirement,
                },
                status: phoxal_cli_core::session::supervisor::ProcessStatus {
                    actual: process_state(status.state),
                    ..Default::default()
                },
            },
        );
        actor.publish(&self.snapshot_tx);
    }

    #[cfg(test)]
    pub fn upsert(&self, status: ParticipantStatus) {
        let key = ProcessKey::from(status.id.as_str());
        self.upsert_process(key, status, StartupRequirement::Required);
    }

    pub(crate) fn register_planned(
        &self,
        key: &ProcessKey,
        kind: ParticipantKind,
        startup_requirement: StartupRequirement,
    ) {
        if self
            .state
            .lock()
            .expect("supervisor state mutex poisoned")
            .snapshot
            .processes
            .contains_key(key)
        {
            return;
        }
        self.upsert_process(
            key.clone(),
            ParticipantStatus::new(key.to_string(), kind, ParticipantState::Starting),
            startup_requirement,
        );
    }

    pub fn set_state(
        &self,
        key: impl Into<ProcessKey>,
        state: ParticipantState,
        note: Option<String>,
    ) {
        let key = key.into();
        let id = key.to_string();
        let mut snapshot = self.inner.lock().expect("board mutex poisoned");
        if let Some(status) = snapshot.participants.get_mut(&id) {
            status.state = state;
            if matches!(
                state,
                ParticipantState::Starting
                    | ParticipantState::Restarting
                    | ParticipantState::Stopped
            ) {
                status.pid = None;
                status.artifact_size_bytes = None;
            }
            if note.is_some() {
                status.note = note.clone();
            }
        }
        drop(snapshot);
        let mut actor = self.state.lock().expect("supervisor state mutex poisoned");
        if let Some(entry) = actor.snapshot.processes.get_mut(&key) {
            entry.status.actual = process_state(state);
            if matches!(
                state,
                ParticipantState::Starting
                    | ParticipantState::Restarting
                    | ParticipantState::Stopped
            ) {
                entry.status.pid = None;
            }
            if state == ParticipantState::Failed {
                entry.status.last_failure = Some(ProcessFailure {
                    kind: failure_kind(note.as_deref()),
                    occurred_at: SystemTime::now(),
                    exit: None,
                    detail: BoundedString::new(note.as_deref().unwrap_or("process failed")),
                    stderr_tail: self.stderr_tail(&key).map(|tail| {
                        BoundedString::with_max_bytes(
                            tail,
                            phoxal_cli_core::session::protocol::MAX_PROCESS_STDERR_TAIL_BYTES,
                        )
                    }),
                });
            }
            actor.publish(&self.snapshot_tx);
        }
    }

    pub fn set_note(&self, key: impl Into<ProcessKey>, note: impl Into<String>) {
        let key = key.into();
        let id = key.to_string();
        let mut snapshot = self.inner.lock().expect("board mutex poisoned");
        if let Some(status) = snapshot.participants.get_mut(&id) {
            status.note = Some(note.into());
        }
    }

    pub fn record_failure(
        &self,
        key: impl Into<ProcessKey>,
        kind: ProcessFailureKind,
        exit: Option<phoxal_cli_core::session::ExitDescription>,
        detail: impl Into<String>,
    ) {
        let key = key.into();
        let detail = detail.into();
        self.set_state(&key, ParticipantState::Failed, Some(detail.clone()));
        let stderr_tail = self.stderr_tail(&key).map(|tail| {
            BoundedString::with_max_bytes(
                tail,
                phoxal_cli_core::session::protocol::MAX_PROCESS_STDERR_TAIL_BYTES,
            )
        });
        let mut actor = self.state.lock().expect("supervisor state mutex poisoned");
        if let Some(entry) = actor.snapshot.processes.get_mut(&key) {
            entry.status.last_failure = Some(ProcessFailure {
                kind,
                occurred_at: SystemTime::now(),
                exit,
                detail: BoundedString::new(detail),
                stderr_tail,
            });
            actor.publish(&self.snapshot_tx);
        }
    }

    pub fn set_restart_count(&self, key: impl Into<ProcessKey>, count: u32) {
        let key = key.into();
        let id = key.to_string();
        let mut snapshot = self.inner.lock().expect("board mutex poisoned");
        if let Some(status) = snapshot.participants.get_mut(&id) {
            status.restart_count = count;
        }
        drop(snapshot);
        let mut actor = self.state.lock().expect("supervisor state mutex poisoned");
        if let Some(entry) = actor.snapshot.processes.get_mut(&key) {
            entry.status.restart_count_in_generation = count;
            entry.status.restart_count_total = entry.status.restart_count_total.saturating_add(1);
            actor.publish(&self.snapshot_tx);
        }
    }

    pub fn append_log(&self, key: impl Into<ProcessKey>, line: impl Into<String>) {
        let key = key.into();
        let _ = self.try_append_log(&key.to_string(), line);
    }

    fn try_append_log(&self, id: &str, line: impl Into<String>) -> bool {
        let line = bounded_log_text(&line.into());
        let mut snapshot = self.inner.lock().expect("board mutex poisoned");
        let Some(status) = snapshot.participants.get_mut(id) else {
            return false;
        };
        status.last_log_line = Some(line.clone());
        status.last_log_lines.push(line);
        const MAX_LAST_LINES: usize = 8;
        if status.last_log_lines.len() > MAX_LAST_LINES {
            let drop_count = status.last_log_lines.len() - MAX_LAST_LINES;
            status.last_log_lines.drain(0..drop_count);
        }
        true
    }

    /// Register the live [`RoutedLogLine`] sink for this session's display
    /// (a TUI). Replaces any previous sink - only one live display exists per
    /// session.
    pub fn set_log_sink(&self, sender: mpsc::Sender<RoutedLogUpdate>) {
        *self.log_sink.lock().expect("log sink mutex poisoned") = Some(sender);
    }

    /// Record one unscoped captured stdout/stderr line. Robot bus records must
    /// use [`Self::route_log_line`] so their typed severity, producer event
    /// time, and robot scope cannot be omitted.
    pub fn route_log(&self, key: impl Into<ProcessKey>, text: impl Into<String>) -> bool {
        let key = key.into();
        self.route_log_line(RoutedLogLine {
            participant: key.to_string(),
            source: LogSource::Raw,
            severity: LogSeverity::Info,
            text: text.into(),
            event_time: SystemTime::now(),
            scope: None,
        })
    }

    /// Route a complete robot-scoped bus record when the producer supplied its
    /// own event time. Retained tool-log replay uses this exclusive bus path so
    /// snapshot replacement does not rewrite chronology or lose robot scope.
    pub fn route_log_line(&self, mut line: RoutedLogLine) -> bool {
        line.text = bounded_log_text(&line.text);
        let id = self
            .display_id_for_bus_line(&line)
            .unwrap_or_else(|| line.participant.clone());
        let source = line.source;
        if !self.try_append_log(&id, line.text.clone()) {
            if source == LogSource::Bus {
                self.disclose_unknown_bus_id(&id, "log");
            }
            return false;
        }
        let sink = self.log_sink.lock().expect("log sink mutex poisoned");
        if let Some(sender) = sink.as_ref() {
            // Non-blocking: a full channel (redraw overdue) or a closed one
            // (no live TUI) both just mean this line never reaches the
            // scrollback - never worth blocking the caller (a bus-log
            // subscriber or output-reader task) over.
            return sender.try_send(RoutedLogUpdate::Append(line)).is_ok();
        }
        true
    }

    /// Install one complete tool-log snapshot as the bus-derived presentation
    /// state. Returns false when the bounded display channel dropped the
    /// replacement so the adapter can query again.
    pub fn replace_bus_logs(&self, scope: LogScope, lines: Vec<RoutedLogLine>) -> bool {
        let mut accepted = Vec::with_capacity(lines.len());
        for mut line in lines {
            let display_id = self
                .display_id_for_bus_line(&line)
                .unwrap_or_else(|| line.participant.clone());
            if self.try_append_log(&display_id, line.text.clone()) {
                line.participant = display_id;
                accepted.push(line);
            } else {
                self.disclose_unknown_bus_id(&line.participant, "log");
            }
        }
        let sink = self.log_sink.lock().expect("log sink mutex poisoned");
        sink.as_ref().is_none_or(|sender| {
            sender
                .try_send(RoutedLogUpdate::Replace {
                    scope,
                    lines: accepted,
                })
                .is_ok()
        })
    }

    fn disclose_unknown_bus_id(&self, id: &str, signal: &'static str) {
        const MAX_UNKNOWN_IDS: usize = 64;
        const MAX_UNKNOWN_ID_CHARS: usize = 128;
        let id = bounded_chars(id, MAX_UNKNOWN_ID_CHARS);
        let mut disclosed = self
            .unknown_bus_ids
            .lock()
            .expect("unknown bus id mutex poisoned");
        if disclosed.contains(&id) || disclosed.len() >= MAX_UNKNOWN_IDS {
            return;
        }
        disclosed.insert(id.clone());
        drop(disclosed);
        tracing::warn!(participant = %id, signal, "ignored bus traffic from an unplanned participant");
    }

    fn display_id_for_bus_line(&self, line: &RoutedLogLine) -> Option<String> {
        let scope = line.scope.as_ref()?;
        let key = ProcessKey::robot(
            RobotKey::new(&scope.namespace, &scope.robot_id),
            &line.participant,
        );
        self.state
            .lock()
            .expect("supervisor state mutex poisoned")
            .snapshot
            .processes
            .contains_key(&key)
            .then(|| key.to_string())
    }

    fn stderr_tail(&self, key: &ProcessKey) -> Option<String> {
        self.inner
            .lock()
            .expect("board mutex poisoned")
            .participants
            .get(&key.to_string())
            .and_then(|status| {
                let tail = status
                    .last_log_lines
                    .iter()
                    .filter(|line| line.starts_with("stderr:"))
                    .cloned()
                    .collect::<Vec<_>>()
                    .join("\n");
                (!tail.is_empty()).then_some(tail)
            })
    }

    pub fn set_lifecycle(&self, lifecycle: ProjectLifecycle) {
        let mut actor = self.state.lock().expect("supervisor state mutex poisoned");
        actor.snapshot.lifecycle = lifecycle;
        actor.publish(&self.snapshot_tx);
    }

    pub fn configure(
        &self,
        project: impl Into<String>,
        framework_train: impl Into<String>,
        execution: impl Into<String>,
    ) {
        let mut actor = self.state.lock().expect("supervisor state mutex poisoned");
        actor.snapshot.project = bounded_snapshot_text(&project.into());
        actor.snapshot.entry = bounded_snapshot_text(
            &phoxal_cli_core::project::resolver::discover_robot_yaml(std::path::Path::new(
                &actor.snapshot.project,
            ))
            .unwrap_or_else(|_| std::path::Path::new(&actor.snapshot.project).join("robot.yaml"))
            .display()
            .to_string(),
        );
        actor.snapshot.framework_train = bounded_snapshot_text(&framework_train.into());
        actor.snapshot.execution = bounded_snapshot_text(&execution.into());
        actor.snapshot.plan_revision = 1;
        actor.publish(&self.snapshot_tx);
    }

    pub fn set_router_status(&self, status: impl Into<String>) {
        let mut actor = self.state.lock().expect("supervisor state mutex poisoned");
        actor.snapshot.router = bounded_snapshot_text(&status.into());
        actor.publish(&self.snapshot_tx);
    }

    pub fn set_simulation_info(&self, profile: impl Into<String>, world: impl Into<String>) {
        let mut actor = self.state.lock().expect("supervisor state mutex poisoned");
        actor.snapshot.simulation = Some(phoxal_cli_core::session::SimulationSessionInfo {
            profile: bounded_snapshot_text(&profile.into()),
            world: bounded_snapshot_text(&world.into()),
        });
        actor.publish(&self.snapshot_tx);
    }

    pub fn begin_phase(&self, phase: &str) {
        let mut actor = self.state.lock().expect("supervisor state mutex poisoned");
        actor.snapshot.startup.active_phase = Some(bounded_snapshot_text(phase));
        actor.publish(&self.snapshot_tx);
    }

    pub fn complete_phase(&self, phase: &str) {
        let mut actor = self.state.lock().expect("supervisor state mutex poisoned");
        actor.snapshot.startup.active_phase = None;
        actor
            .snapshot
            .startup
            .completed_phases
            .push(bounded_snapshot_text(phase));
        let maximum = phoxal_cli_core::session::protocol::MAX_STARTUP_PHASES;
        if actor.snapshot.startup.completed_phases.len() > maximum {
            let remove = actor.snapshot.startup.completed_phases.len() - maximum;
            actor.snapshot.startup.completed_phases.drain(..remove);
        }
        actor.publish(&self.snapshot_tx);
    }

    #[must_use]
    pub fn process_state(&self, key: &ProcessKey) -> Option<ProcessState> {
        self.state
            .lock()
            .expect("supervisor state mutex poisoned")
            .snapshot
            .processes
            .get(key)
            .map(|entry| entry.status.actual)
    }

    pub fn set_launch_command(&self, id: &str, command: ParticipantLaunchCommand) {
        let mut snapshot = self.inner.lock().expect("board mutex poisoned");
        if let Some(status) = snapshot.participants.get_mut(id) {
            status.launch_command = Some(command);
        }
    }

    pub fn set_process_details(
        &self,
        key: impl Into<ProcessKey>,
        pid: Option<u32>,
        artifact_size_bytes: Option<u64>,
    ) {
        let key = key.into();
        let id = key.to_string();
        let mut snapshot = self.inner.lock().expect("board mutex poisoned");
        if let Some(status) = snapshot.participants.get_mut(&id) {
            status.pid = pid;
            status.artifact_size_bytes = artifact_size_bytes;
        }
        drop(snapshot);
        let mut actor = self.state.lock().expect("supervisor state mutex poisoned");
        if let Some(entry) = actor.snapshot.processes.get_mut(&key) {
            entry.status.pid = pid;
            actor.publish(&self.snapshot_tx);
        }
    }

    pub fn set_launch_command_for(&self, key: &ProcessKey, command: ParticipantLaunchCommand) {
        self.set_launch_command(&key.to_string(), command);
    }

    pub fn set_incarnation(&self, key: &ProcessKey, incarnation: u64) {
        let mut actor = self.state.lock().expect("supervisor state mutex poisoned");
        if let Some(entry) = actor.snapshot.processes.get_mut(key) {
            entry.status.incarnation = Some(incarnation);
            actor.publish(&self.snapshot_tx);
        }
    }

    pub fn mint_incarnation(&self) -> u64 {
        let mut actor = self.state.lock().expect("supervisor state mutex poisoned");
        loop {
            let incarnation = random_nonzero_u64();
            if actor.used_incarnations.insert(incarnation) {
                return incarnation;
            }
        }
    }

    #[must_use]
    pub fn supervisor_snapshot(&self) -> SupervisorSnapshotV0 {
        self.state
            .lock()
            .expect("supervisor state mutex poisoned")
            .snapshot
            .clone()
    }

    pub fn subscribe(&self) -> watch::Receiver<SupervisorSnapshotV0> {
        self.snapshot_tx.subscribe()
    }

    #[must_use]
    pub fn snapshot(&self) -> BoardSnapshot {
        let mut board = self.inner.lock().expect("board mutex poisoned").clone();
        let supervisor = self.supervisor_snapshot();
        for (key, entry) in supervisor.processes {
            if let Some(status) = board.participants.get_mut(&key.to_string()) {
                status.state = participant_state(entry.status.actual);
                status.pid = entry.status.pid;
                status.restart_count = entry.status.restart_count_in_generation;
            }
        }
        board
    }
}

fn process_state(state: ParticipantState) -> ProcessState {
    match state {
        ParticipantState::Starting => ProcessState::Starting,
        ParticipantState::Ready => ProcessState::Ready,
        ParticipantState::Degraded => ProcessState::Degraded,
        ParticipantState::Failed => ProcessState::Failed,
        ParticipantState::Restarting => ProcessState::Restarting,
        ParticipantState::Stopped => ProcessState::Stopped,
    }
}

fn participant_state(state: ProcessState) -> ParticipantState {
    match state {
        ProcessState::Starting => ParticipantState::Starting,
        ProcessState::Ready => ParticipantState::Ready,
        ProcessState::Degraded => ParticipantState::Degraded,
        ProcessState::Failed => ParticipantState::Failed,
        ProcessState::Restarting => ParticipantState::Restarting,
        ProcessState::Stopped => ParticipantState::Stopped,
    }
}

fn failure_kind(note: Option<&str>) -> ProcessFailureKind {
    match note.unwrap_or_default() {
        note if note.contains("spawn") => ProcessFailureKind::Spawn,
        note if note.contains("timed out") => ProcessFailureKind::ReadinessTimeout,
        note if note.contains("stop") || note.contains("cleanup") => ProcessFailureKind::Cleanup,
        note if note.contains("exit") || note.contains("status") => ProcessFailureKind::Exit,
        _ => ProcessFailureKind::Other,
    }
}

fn random_nonzero_u64() -> u64 {
    loop {
        let mut bytes = [0_u8; 8];
        getrandom::fill(&mut bytes).expect("operating-system CSPRNG unavailable");
        let value = u64::from_ne_bytes(bytes);
        if value != 0 {
            return value;
        }
    }
}

fn bounded_snapshot_text(value: &str) -> String {
    let maximum = phoxal_cli_core::session::protocol::MAX_SNAPSHOT_TEXT_BYTES;
    if value.len() <= maximum {
        return value.to_string();
    }
    let suffix = "…";
    let mut end = maximum.saturating_sub(suffix.len());
    while !value.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    format!("{}{}", &value[..end], suffix)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn liveliness_presence_is_explicit_and_recovery_resets_it_to_unknown() {
        let board = BoardBackend::new();
        let key = ProcessKey::project("drive");
        board.register_planned(&key, ParticipantKind::Service, StartupRequirement::Required);

        board.record_presence("drive", true);
        assert_eq!(board.snapshot().participants["drive"].present, Some(true));
        board.record_presence("drive", false);
        assert_eq!(board.snapshot().participants["drive"].present, Some(false));

        board.begin_recovery_epoch(&[(key, None)], &[]);
        assert_eq!(board.snapshot().participants["drive"].present, None);
    }

    #[test]
    fn scoped_processes_with_the_same_participant_id_do_not_collide() {
        let board = BoardBackend::new();
        let left = ProcessKey::robot(RobotKey::new("lab", "alpha"), "motion");
        let right = ProcessKey::robot(RobotKey::new("lab", "beta"), "motion");
        for key in [&left, &right] {
            board.upsert_process(
                key.clone(),
                ParticipantStatus::new(
                    "motion",
                    ParticipantKind::Service,
                    ParticipantState::Starting,
                ),
                StartupRequirement::Required,
            );
        }
        board.set_restart_count(&left, 1);
        board.set_restart_count(&right, 3);

        let supervisor = board.supervisor_snapshot();
        assert_eq!(supervisor.processes.len(), 2);
        assert_eq!(
            supervisor.processes[&left]
                .status
                .restart_count_in_generation,
            1
        );
        assert_eq!(
            supervisor.processes[&right]
                .status
                .restart_count_in_generation,
            3
        );
        assert_eq!(board.snapshot().participants.len(), 2);
    }

    #[test]
    fn exact_incarnation_readiness_rejects_stale_holder_and_aggregates_presence() {
        let board = BoardBackend::new();
        let robot = RobotKey::new("lab", "rover");
        let key = ProcessKey::robot(robot.clone(), "mission");
        board.upsert_process(
            key.clone(),
            ParticipantStatus::new(
                "mission",
                ParticipantKind::Service,
                ParticipantState::Starting,
            ),
            StartupRequirement::Required,
        );
        board.set_incarnation(&key, 22);
        let instance = |incarnation| ParticipantInstanceKey {
            robot: robot.clone(),
            participant: "mission".to_string(),
            incarnation,
        };

        board.record_instance_presence(instance(11), true);
        assert_eq!(board.process_state(&key), Some(ProcessState::Starting));
        assert_eq!(
            board.snapshot().participants["lab/rover::mission"].present,
            Some(true)
        );
        board.record_instance_presence(instance(22), true);
        assert_eq!(board.process_state(&key), Some(ProcessState::Ready));
        board.record_instance_presence(instance(22), false);
        assert_eq!(board.process_state(&key), Some(ProcessState::Ready));
        assert_eq!(
            board.snapshot().participants["lab/rover::mission"].present,
            Some(true)
        );
        board.record_instance_presence(instance(11), false);
        assert_eq!(
            board.snapshot().participants["lab/rover::mission"].present,
            Some(false)
        );
        assert_eq!(board.process_state(&key), Some(ProcessState::Ready));
    }

    #[tokio::test]
    async fn watch_snapshots_are_full_revisioned_values_and_slow_consumers_converge() {
        let board = BoardBackend::new();
        let mut consumer = board.subscribe();
        let key = ProcessKey::project("router");
        board.upsert_process(
            key.clone(),
            ParticipantStatus::new("router", ParticipantKind::Tool, ParticipantState::Starting),
            StartupRequirement::Required,
        );
        board.set_state(&key, ParticipantState::Ready, None);
        board.set_lifecycle(ProjectLifecycle::Ready);

        consumer.changed().await.expect("snapshot sender lives");
        let latest = consumer.borrow_and_update().clone();
        assert_eq!(latest.lifecycle, ProjectLifecycle::Ready);
        assert_eq!(latest.processes[&key].status.actual, ProcessState::Ready);
        assert!(latest.revision >= 3);
    }

    #[test]
    fn failed_process_retains_incarnation_and_bounded_pre_bus_evidence() {
        let board = BoardBackend::new();
        let key = ProcessKey::project("early-crash");
        board.upsert_process(
            key.clone(),
            ParticipantStatus::new(
                "early-crash",
                ParticipantKind::Service,
                ParticipantState::Starting,
            ),
            StartupRequirement::Required,
        );
        board.set_incarnation(&key, 91);
        board.append_log(&key, format!("stderr: {}", "x".repeat(10_000)));
        board.record_failure(
            &key,
            ProcessFailureKind::Exit,
            Some(phoxal_cli_core::session::ExitDescription {
                code: Some(7),
                signal: None,
            }),
            "process exited before opening the bus",
        );
        let snapshot = board.supervisor_snapshot();
        let status = &snapshot.processes[&key].status;
        assert_eq!(status.incarnation, Some(91));
        let failure = status.last_failure.as_ref().expect("failure evidence");
        assert_eq!(failure.exit.as_ref().and_then(|exit| exit.code), Some(7));
        assert!(
            failure
                .stderr_tail
                .as_ref()
                .is_some_and(|tail| tail.as_str().len() <= BoundedString::MAX_BYTES)
        );
    }

    #[test]
    fn minted_incarnations_are_nonzero_and_collision_checked() {
        let board = BoardBackend::new();
        let values = (0..1_024)
            .map(|_| board.mint_incarnation())
            .collect::<BTreeSet<_>>();
        assert_eq!(values.len(), 1_024);
        assert!(!values.contains(&0));
    }
}
