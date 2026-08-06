use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use phoxal_cli_core::identity::ProducerId;
use phoxal_cli_core::runtime::{
    BoundedString, ExitDescription, ParticipantInstanceKey, ParticipantKind, ProcessDescriptor,
    ProcessEntry, ProcessFailure, ProcessFailureKind, ProcessKey, ProcessState, ProjectLifecycle,
    StartupRequirement,
};
use phoxal_supervisor_api::{Detail, StderrTail};
use tokio::sync::watch;

use super::Board;

/// The daemon's authoritative process and lifecycle state.
///
/// It is typed on the internal [`Board`], never on a wire document: the
/// execution's identity, mode, startup sequence, and typed failure are the
/// daemon's own facts and live in `daemon::state`, and disposable attachment
/// projections, retained observations, logs, telemetry, and UI channels live
/// outside the daemon entirely.
#[derive(Debug, Clone)]
pub struct SupervisorState {
    board: Arc<Mutex<Board>>,
    publisher: watch::Sender<Board>,
    exact_instances: Arc<Mutex<ExactReadiness>>,
    captured_stderr: Arc<Mutex<HashMap<ProcessKey, VecDeque<String>>>>,
}

#[derive(Debug, Default)]
struct ExactReadiness {
    enabled: bool,
    instances: HashSet<ParticipantInstanceKey>,
}

impl Default for SupervisorState {
    fn default() -> Self {
        let board = Board::default();
        let (publisher, _) = watch::channel(board.clone());
        Self {
            board: Arc::new(Mutex::new(board)),
            publisher,
            exact_instances: Arc::new(Mutex::new(ExactReadiness {
                enabled: true,
                instances: HashSet::new(),
            })),
            captured_stderr: Arc::default(),
        }
    }
}

impl SupervisorState {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Consume a participant's liveliness token as startup proof, and as the
    /// one place the supervisor learns which producer this incarnation
    /// actually publishes under.
    pub fn record_instance_presence(
        &self,
        instance: ParticipantInstanceKey,
        producer: ProducerId,
        present: bool,
    ) {
        let mut readiness = self
            .exact_instances
            .lock()
            .expect("exact readiness mutex poisoned");
        if !readiness.enabled {
            return;
        }
        if present {
            readiness.instances.insert(instance.clone());
        } else {
            readiness.instances.remove(&instance);
            return;
        }
        drop(readiness);

        let robot_key = ProcessKey::robot(instance.robot.clone(), instance.participant.clone());
        let project_key = ProcessKey::project(&instance.participant);
        let key = {
            let board = self.board.lock().expect("supervisor board mutex poisoned");
            if board.processes.contains_key(&robot_key) {
                robot_key
            } else if board.processes.contains_key(&project_key) {
                project_key
            } else {
                return;
            }
        };
        let starting = self
            .board
            .lock()
            .expect("supervisor board mutex poisoned")
            .processes
            .get(&key)
            .is_some_and(|entry| entry.status.actual == ProcessState::Starting);
        if starting {
            // The token is the first evidence this incarnation has a session,
            // so it carries both facts at once: which producer it publishes
            // under, and that it is ready.
            self.set_producer(&key, producer);
            self.set_state(key, ProcessState::Ready, None);
        }
    }

    #[must_use]
    pub fn is_exact_present(&self, instance: &ParticipantInstanceKey) -> bool {
        self.exact_instances
            .lock()
            .expect("exact readiness mutex poisoned")
            .instances
            .contains(instance)
    }

    pub fn upsert_process(
        &self,
        key: ProcessKey,
        kind: ParticipantKind,
        state: ProcessState,
        startup_requirement: StartupRequirement,
    ) {
        self.modify(|board| {
            let artifact = key.to_string();
            board.processes.insert(
                key.clone(),
                ProcessEntry {
                    descriptor: ProcessDescriptor {
                        key,
                        kind,
                        artifact,
                        owner: "phoxal-cli".to_string(),
                        startup_requirement,
                    },
                    status: phoxal_cli_core::runtime::ProcessStatus {
                        actual: state,
                        ..Default::default()
                    },
                },
            );
        });
    }

    pub(crate) fn register_planned(
        &self,
        key: &ProcessKey,
        kind: ParticipantKind,
        startup_requirement: StartupRequirement,
    ) {
        if self
            .board
            .lock()
            .expect("supervisor board mutex poisoned")
            .processes
            .contains_key(key)
        {
            return;
        }
        self.upsert_process(
            key.clone(),
            kind,
            ProcessState::Starting,
            startup_requirement,
        );
    }

    pub fn set_state(
        &self,
        key: impl Into<ProcessKey>,
        state: ProcessState,
        failure_detail: Option<String>,
    ) {
        let key = key.into();
        let stderr_tail = (state == ProcessState::Failed)
            .then(|| self.stderr_tail(&key))
            .flatten();
        self.modify(|board| {
            if let Some(entry) = board.processes.get_mut(&key) {
                entry.status.actual = state;
                if matches!(
                    state,
                    ProcessState::Starting | ProcessState::Restarting | ProcessState::Stopped
                ) {
                    entry.status.pid = None;
                }
                if state == ProcessState::Failed {
                    entry.status.last_failure = Some(ProcessFailure {
                        kind: failure_kind(failure_detail.as_deref()),
                        occurred_at: SystemTime::now(),
                        exit: None,
                        detail: BoundedString::new(
                            failure_detail.as_deref().unwrap_or("process failed"),
                        ),
                        stderr_tail,
                    });
                }
            }
        });
    }

    pub fn record_failure(
        &self,
        key: impl Into<ProcessKey>,
        kind: ProcessFailureKind,
        exit: Option<ExitDescription>,
        detail: impl Into<String>,
    ) {
        let key = key.into();
        let detail = detail.into();
        let stderr_tail = self.stderr_tail(&key);
        self.modify(|board| {
            if let Some(entry) = board.processes.get_mut(&key) {
                entry.status.actual = ProcessState::Failed;
                entry.status.last_failure = Some(ProcessFailure {
                    kind,
                    occurred_at: SystemTime::now(),
                    exit,
                    detail: BoundedString::new(detail),
                    stderr_tail,
                });
            }
        });
    }

    pub fn set_restart_count(&self, key: impl Into<ProcessKey>, count: u32) {
        let key = key.into();
        self.modify(|board| {
            if let Some(entry) = board.processes.get_mut(&key) {
                entry.status.restart_count_in_generation = count;
                entry.status.restart_count_total =
                    entry.status.restart_count_total.saturating_add(1);
            }
        });
    }

    pub fn set_pid(&self, key: impl Into<ProcessKey>, pid: Option<u32>) {
        let key = key.into();
        self.modify(|board| {
            if let Some(entry) = board.processes.get_mut(&key) {
                entry.status.pid = pid;
            }
        });
    }

    /// Retain bounded stderr solely as resident-owned failure evidence.
    pub(crate) fn record_captured_stderr(&self, key: &ProcessKey, line: &str) {
        const MAX_LINES: usize = 8;
        let line = BoundedString::with_max_bytes(line, StderrTail::MAX_BYTES)
            .as_str()
            .to_string();
        let mut stderr = self
            .captured_stderr
            .lock()
            .expect("captured stderr mutex poisoned");
        let lines = stderr.entry(key.clone()).or_default();
        lines.push_back(line);
        while lines.len() > MAX_LINES {
            lines.pop_front();
        }
    }

    pub(crate) fn clear_captured_stderr(&self, key: &ProcessKey) {
        self.captured_stderr
            .lock()
            .expect("captured stderr mutex poisoned")
            .remove(key);
    }

    pub fn set_producer(&self, key: &ProcessKey, producer: ProducerId) {
        self.modify(|board| {
            if let Some(entry) = board.processes.get_mut(key) {
                entry.status.producer = Some(producer);
            }
        });
    }

    /// Forget the producer of a process that is being (re)spawned. A fresh
    /// incarnation has not opened its session yet, so it has no producer to
    /// report - and a restart fenced on the previous one must not match.
    pub fn clear_producer(&self, key: &ProcessKey) {
        self.modify(|board| {
            if let Some(entry) = board.processes.get_mut(key) {
                entry.status.producer = None;
            }
        });
    }

    pub fn set_lifecycle(&self, lifecycle: ProjectLifecycle) {
        self.modify(|board| board.lifecycle = lifecycle);
    }

    /// Record the lifecycle as `Failed` together with the reason, in one
    /// atomic update so a subscriber can never observe `Failed` without the
    /// reason that caused it. This is the resident-level failure (a
    /// preparation or supervision error with no single process to blame);
    /// a single process's own failure is recorded on its `ProcessFailure`
    /// via [`Self::set_state`] or [`Self::record_failure`] instead.
    ///
    /// First cause wins: `lifecycle` is always set to `Failed`, but `failure`
    /// is only ever set once, from `None` to `Some`. A supervision loop can
    /// hit several failure sites once it starts unwinding (a stalled stage,
    /// then every remaining participant's own poll error, ...); only the
    /// first one is the actual cause, and every call site can call `fail`
    /// unconditionally without racing a `lifecycle != Failed` guard against
    /// this method's own writes.
    pub fn fail(&self, reason: &str) {
        let reason = BoundedString::with_max_bytes(reason, Detail::MAX_BYTES)
            .as_str()
            .to_string();
        self.modify(|board| {
            board.lifecycle = ProjectLifecycle::Failed;
            if board.failure.is_none() {
                board.failure = Some(reason);
            }
        });
    }

    #[must_use]
    pub fn process_state(&self, key: &ProcessKey) -> Option<ProcessState> {
        self.board
            .lock()
            .expect("supervisor board mutex poisoned")
            .processes
            .get(key)
            .map(|entry| entry.status.actual)
    }

    #[must_use]
    pub fn snapshot(&self) -> Board {
        self.board
            .lock()
            .expect("supervisor board mutex poisoned")
            .clone()
    }

    pub fn subscribe(&self) -> watch::Receiver<Board> {
        self.publisher.subscribe()
    }

    fn modify(&self, update: impl FnOnce(&mut Board)) {
        let mut board = self.board.lock().expect("supervisor board mutex poisoned");
        update(&mut board);
        board.revision = board.revision.saturating_add(1);
        self.publisher.send_replace(board.clone());
    }

    fn stderr_tail(&self, key: &ProcessKey) -> Option<BoundedString> {
        self.captured_stderr
            .lock()
            .expect("captured stderr mutex poisoned")
            .get(key)
            .map(|lines| lines.iter().cloned().collect::<Vec<_>>().join("\n"))
            .filter(|tail| !tail.is_empty())
            .map(|tail| BoundedString::with_max_bytes(tail, StderrTail::MAX_BYTES))
    }
}

fn failure_kind(detail: Option<&str>) -> ProcessFailureKind {
    match detail.unwrap_or_default() {
        detail if detail.contains("spawn") => ProcessFailureKind::Spawn,
        detail if detail.contains("timed out") => ProcessFailureKind::ReadinessTimeout,
        detail if detail.contains("stop") || detail.contains("cleanup") => {
            ProcessFailureKind::Cleanup
        }
        detail if detail.contains("exit") || detail.contains("status") => ProcessFailureKind::Exit,
        _ => ProcessFailureKind::Other,
    }
}

#[cfg(test)]
mod tests {
    use phoxal_cli_core::runtime::{ParticipantKind, RobotKey};

    use super::*;

    fn producer(seed: u8) -> ProducerId {
        // A producer id is the publisher's session ZID: 1 to 32 lowercase hex
        // characters with no leading zero, so a seed renders bare.
        ProducerId::parse(&format!("{seed:x}")).expect("test producer id must parse")
    }

    #[test]
    fn exact_producer_readiness_rejects_stale_holder() {
        let state = SupervisorState::new();
        let robot = RobotKey::new("lab", "rover");
        let key = ProcessKey::robot(robot.clone(), "mission");
        state.upsert_process(
            key.clone(),
            ParticipantKind::Service,
            ProcessState::Starting,
            StartupRequirement::Required,
        );
        let instance = ParticipantInstanceKey {
            robot: robot.clone(),
            participant: "mission".to_string(),
        };
        // A starting process has no producer at all: nothing mints one, so the
        // snapshot reports "no session yet" rather than an intention.
        assert_eq!(state.snapshot().processes[&key].status.producer, None);

        // The liveliness token is where the producer is learned, and it makes
        // the process ready in the same update.
        state.record_instance_presence(instance.clone(), producer(22), true);
        assert_eq!(state.process_state(&key), Some(ProcessState::Ready));
        assert_eq!(
            state.snapshot().processes[&key].status.producer,
            Some(producer(22))
        );

        // Losing the token does not un-ready a running process.
        state.record_instance_presence(instance, producer(22), false);
        assert_eq!(state.process_state(&key), Some(ProcessState::Ready));
    }

    #[tokio::test]
    async fn snapshots_are_full_revisioned_values() {
        let state = SupervisorState::new();
        let mut consumer = state.subscribe();
        state.upsert_process(
            ProcessKey::project("router"),
            ParticipantKind::Host,
            ProcessState::Starting,
            StartupRequirement::Required,
        );
        consumer.changed().await.unwrap();
        let revision = consumer.borrow_and_update().revision;
        state.set_lifecycle(ProjectLifecycle::Ready);
        consumer.changed().await.unwrap();
        assert!(consumer.borrow().revision > revision);

        for lifecycle in [
            ProjectLifecycle::Degraded,
            ProjectLifecycle::Starting,
            ProjectLifecycle::Ready,
        ] {
            state.set_lifecycle(lifecycle);
        }
        consumer.changed().await.unwrap();
        assert_eq!(
            consumer.borrow_and_update().revision,
            state.snapshot().revision
        );
    }

    #[test]
    fn failures_are_bounded_and_published() {
        let state = SupervisorState::new();
        let key = ProcessKey::project("worker");
        state.upsert_process(
            key.clone(),
            ParticipantKind::Service,
            ProcessState::Starting,
            StartupRequirement::Required,
        );
        state.set_producer(&key, producer(91));
        state.record_captured_stderr(&key, &"x".repeat(10_000));
        state.record_failure(&key, ProcessFailureKind::Exit, None, "x".repeat(10_000));
        let snapshot = state.snapshot();
        let entry = &snapshot.processes[&key];
        let failure = entry.status.last_failure.clone().unwrap();
        assert!(failure.detail.as_str().len() <= BoundedString::FAILURE_MAX_BYTES);
        assert_eq!(entry.status.producer, Some(producer(91)));
        assert!(
            failure
                .stderr_tail
                .as_ref()
                .is_some_and(|tail| !tail.as_str().is_empty())
        );
        assert!(failure.stderr_tail.unwrap().as_str().len() <= StderrTail::MAX_BYTES);
    }

    #[tokio::test]
    async fn fail_sets_lifecycle_and_reason_in_one_atomic_update() {
        let state = SupervisorState::new();
        let mut consumer = state.subscribe();
        state.fail("catalog train floor not supported: 0.41.2 < 0.42.0");
        consumer.changed().await.unwrap();
        let snapshot = consumer.borrow_and_update().clone();
        assert_eq!(snapshot.lifecycle, ProjectLifecycle::Failed);
        assert_eq!(
            snapshot.failure.as_deref(),
            Some("catalog train floor not supported: 0.41.2 < 0.42.0")
        );
    }

    #[test]
    fn fail_keeps_the_first_reason_across_repeated_calls() {
        let state = SupervisorState::new();
        state.fail("stage 'router' stalled: connection refused");
        state.fail("participant 'drive' exhausted its restart policy");
        let snapshot = state.snapshot();
        assert_eq!(snapshot.lifecycle, ProjectLifecycle::Failed);
        assert_eq!(
            snapshot.failure.as_deref(),
            Some("stage 'router' stalled: connection refused"),
            "the first call's reason must survive every later call"
        );
    }

    #[test]
    fn fail_bounds_an_oversized_reason() {
        let state = SupervisorState::new();
        state.fail(&"x".repeat(1_000_000));
        let snapshot = state.snapshot();
        assert_eq!(snapshot.lifecycle, ProjectLifecycle::Failed);
        assert!(snapshot.failure.expect("reason recorded").len() <= Detail::MAX_BYTES);
    }

    #[test]
    fn stderr_failure_evidence_is_scoped_to_one_process_incarnation() {
        let state = SupervisorState::new();
        let key = ProcessKey::project("worker");
        state.record_captured_stderr(&key, "previous incarnation panic");
        state.clear_captured_stderr(&key);
        assert_eq!(state.stderr_tail(&key), None);
    }

    #[test]
    fn failure_details_preserve_their_specific_failure_kind() {
        for (detail, expected) in [
            ("spawn failed: missing binary", ProcessFailureKind::Spawn),
            (
                "timed out waiting for exact readiness",
                ProcessFailureKind::ReadinessTimeout,
            ),
            (
                "exited independently during requested stop (status 1)",
                ProcessFailureKind::Cleanup,
            ),
            ("process exited with status 1", ProcessFailureKind::Exit),
            ("unexpected supervisor condition", ProcessFailureKind::Other),
        ] {
            assert_eq!(failure_kind(Some(detail)), expected, "{detail}");
        }
    }

    #[test]
    fn scoped_processes_with_the_same_participant_id_do_not_collide() {
        let state = SupervisorState::new();
        let left = ProcessKey::robot(RobotKey::new("lab", "left"), "drive");
        let right = ProcessKey::robot(RobotKey::new("lab", "right"), "drive");
        for key in [&left, &right] {
            state.upsert_process(
                key.clone(),
                ParticipantKind::Service,
                ProcessState::Starting,
                StartupRequirement::Required,
            );
        }
        state.set_restart_count(&left, 1);
        state.set_restart_count(&right, 3);
        let snapshot = state.snapshot();
        assert_eq!(
            snapshot.processes[&left].status.restart_count_in_generation,
            1
        );
        assert_eq!(
            snapshot.processes[&right]
                .status
                .restart_count_in_generation,
            3
        );
    }
}
