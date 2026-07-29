use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use phoxal_cli_core::identity::{ExecutionId, ProducerId};
use phoxal_cli_core::session::{
    BoundedString, ExitDescription, ParticipantInstanceKey, ParticipantKind, ProcessDescriptor,
    ProcessEntry, ProcessFailure, ProcessFailureKind, ProcessKey, ProcessState, ProjectLifecycle,
    StartupRequirement,
};
use phoxal_cli_protocol::SupervisorSnapshotV0;
use tokio::sync::watch;

use super::snapshot::{bounded_text, initial_snapshot};

/// The resident's authoritative process and lifecycle state.
///
/// Disposable attachment projections, retained observations, logs, telemetry,
/// and UI channels deliberately live outside this type.
#[derive(Debug, Clone)]
pub struct SupervisorState {
    snapshot: Arc<Mutex<SupervisorSnapshotV0>>,
    publisher: watch::Sender<SupervisorSnapshotV0>,
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
        let snapshot = initial_snapshot();
        let (publisher, _) = watch::channel(snapshot.clone());
        Self {
            snapshot: Arc::new(Mutex::new(snapshot)),
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

    /// Consume exact producer Liveliness only as startup proof.
    pub fn record_instance_presence(&self, instance: ParticipantInstanceKey, present: bool) {
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
            let snapshot = self
                .snapshot
                .lock()
                .expect("supervisor state mutex poisoned");
            if snapshot.processes.contains_key(&robot_key) {
                robot_key
            } else if snapshot.processes.contains_key(&project_key) {
                project_key
            } else {
                return;
            }
        };
        let exact_starting = self
            .snapshot
            .lock()
            .expect("supervisor state mutex poisoned")
            .processes
            .get(&key)
            .is_some_and(|entry| {
                entry.status.producer == Some(instance.producer)
                    && entry.status.actual == ProcessState::Starting
            });
        if exact_starting {
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

    pub(crate) fn begin_recovery_epoch(
        &self,
        spawned: &[(ProcessKey, Option<String>)],
        wait_only: &[ProcessKey],
    ) -> u64 {
        let mut readiness = self
            .exact_instances
            .lock()
            .expect("exact readiness mutex poisoned");
        readiness.enabled = false;
        readiness.instances.clear();
        drop(readiness);

        let generation = {
            let mut snapshot = self
                .snapshot
                .lock()
                .expect("supervisor state mutex poisoned");
            for key in spawned.iter().map(|(key, _)| key).chain(wait_only) {
                if let Some(entry) = snapshot.processes.get_mut(key) {
                    entry.status.actual = ProcessState::Starting;
                    entry.status.pid = None;
                    entry.status.producer = None;
                    entry.status.restart_count_in_generation = 0;
                }
            }
            snapshot.graph_generation = snapshot.graph_generation.saturating_add(1);
            snapshot.lifecycle = ProjectLifecycle::Starting;
            snapshot.startup.active_phase = None;
            snapshot.startup.completed_phases.clear();
            snapshot.revision = snapshot.revision.saturating_add(1);
            let generation = snapshot.graph_generation;
            self.publisher.send_replace(snapshot.clone());
            generation
        };
        let mut stderr = self
            .captured_stderr
            .lock()
            .expect("captured stderr mutex poisoned");
        for key in spawned.iter().map(|(key, _)| key).chain(wait_only) {
            stderr.remove(key);
        }
        generation
    }

    pub(crate) fn enable_presence_for_recovery(&self) {
        self.exact_instances
            .lock()
            .expect("exact readiness mutex poisoned")
            .enabled = true;
    }

    pub fn upsert_process(
        &self,
        key: ProcessKey,
        kind: ParticipantKind,
        state: ProcessState,
        startup_requirement: StartupRequirement,
    ) {
        self.modify(|snapshot| {
            let artifact = key.to_string();
            snapshot.processes.insert(
                key.clone(),
                ProcessEntry {
                    descriptor: ProcessDescriptor {
                        key,
                        kind,
                        artifact,
                        owner: "phoxal-cli".to_string(),
                        startup_requirement,
                    },
                    status: phoxal_cli_core::session::supervisor::ProcessStatus {
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
            .snapshot
            .lock()
            .expect("supervisor state mutex poisoned")
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
        self.modify(|snapshot| {
            if let Some(entry) = snapshot.processes.get_mut(&key) {
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
        self.modify(|snapshot| {
            if let Some(entry) = snapshot.processes.get_mut(&key) {
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
        self.modify(|snapshot| {
            if let Some(entry) = snapshot.processes.get_mut(&key) {
                entry.status.restart_count_in_generation = count;
                entry.status.restart_count_total =
                    entry.status.restart_count_total.saturating_add(1);
            }
        });
    }

    pub fn set_pid(&self, key: impl Into<ProcessKey>, pid: Option<u32>) {
        let key = key.into();
        self.modify(|snapshot| {
            if let Some(entry) = snapshot.processes.get_mut(&key) {
                entry.status.pid = pid;
            }
        });
    }

    /// Retain bounded stderr solely as resident-owned failure evidence.
    pub(crate) fn record_captured_stderr(&self, key: &ProcessKey, line: &str) {
        const MAX_LINES: usize = 8;
        let line = BoundedString::with_max_bytes(
            line,
            phoxal_cli_protocol::limits::MAX_PROCESS_STDERR_TAIL_BYTES,
        )
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
        self.modify(|snapshot| {
            if let Some(entry) = snapshot.processes.get_mut(key) {
                entry.status.producer = Some(producer);
            }
        });
    }

    pub fn set_lifecycle(&self, lifecycle: ProjectLifecycle) {
        self.modify(|snapshot| snapshot.lifecycle = lifecycle);
    }

    pub fn configure(
        &self,
        project: impl Into<String>,
        framework_train: impl Into<String>,
        execution_id: ExecutionId,
        router_endpoint: impl Into<String>,
    ) {
        self.modify(|snapshot| {
            snapshot.project = bounded_text(&project.into());
            snapshot.entry = bounded_text(
                &phoxal_cli_core::project::resolver::discover_robot_yaml(std::path::Path::new(
                    &snapshot.project,
                ))
                .unwrap_or_else(|_| std::path::Path::new(&snapshot.project).join("robot.yaml"))
                .display()
                .to_string(),
            );
            snapshot.framework_train = bounded_text(&framework_train.into());
            snapshot.execution_id = execution_id;
            snapshot.router = bounded_text(&router_endpoint.into());
            snapshot.plan_revision = 1;
        });
    }

    pub fn set_router_endpoint(&self, endpoint: impl Into<String>) {
        self.modify(|snapshot| snapshot.router = bounded_text(&endpoint.into()));
    }

    pub fn set_simulation_info(&self, profile: impl Into<String>, world: impl Into<String>) {
        self.modify(|snapshot| {
            snapshot.simulation = Some(phoxal_cli_core::session::SimulationSessionInfo {
                profile: bounded_text(&profile.into()),
                world: bounded_text(&world.into()),
            });
        });
    }

    pub fn begin_phase(&self, phase: &str) {
        self.modify(|snapshot| snapshot.startup.active_phase = Some(bounded_text(phase)));
    }

    pub fn complete_phase(&self, phase: &str) {
        self.modify(|snapshot| {
            snapshot.startup.active_phase = None;
            snapshot.startup.completed_phases.push(bounded_text(phase));
            let maximum = phoxal_cli_protocol::limits::MAX_STARTUP_PHASES;
            if snapshot.startup.completed_phases.len() > maximum {
                let remove = snapshot.startup.completed_phases.len() - maximum;
                snapshot.startup.completed_phases.drain(..remove);
            }
        });
    }

    #[must_use]
    pub fn process_state(&self, key: &ProcessKey) -> Option<ProcessState> {
        self.snapshot
            .lock()
            .expect("supervisor state mutex poisoned")
            .processes
            .get(key)
            .map(|entry| entry.status.actual)
    }

    #[must_use]
    pub fn supervisor_snapshot(&self) -> SupervisorSnapshotV0 {
        self.snapshot
            .lock()
            .expect("supervisor state mutex poisoned")
            .clone()
    }

    pub fn subscribe(&self) -> watch::Receiver<SupervisorSnapshotV0> {
        self.publisher.subscribe()
    }

    fn modify(&self, update: impl FnOnce(&mut SupervisorSnapshotV0)) {
        let mut snapshot = self
            .snapshot
            .lock()
            .expect("supervisor state mutex poisoned");
        update(&mut snapshot);
        snapshot.revision = snapshot.revision.saturating_add(1);
        self.publisher.send_replace(snapshot.clone());
    }

    fn stderr_tail(&self, key: &ProcessKey) -> Option<BoundedString> {
        self.captured_stderr
            .lock()
            .expect("captured stderr mutex poisoned")
            .get(key)
            .map(|lines| lines.iter().cloned().collect::<Vec<_>>().join("\n"))
            .filter(|tail| !tail.is_empty())
            .map(|tail| {
                BoundedString::with_max_bytes(
                    tail,
                    phoxal_cli_protocol::limits::MAX_PROCESS_STDERR_TAIL_BYTES,
                )
            })
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
    use phoxal_cli_core::session::{ParticipantKind, RobotKey};

    use super::*;

    fn producer(seed: u8) -> ProducerId {
        ProducerId::parse(&format!("{:032x}", u128::from(seed)))
            .expect("test producer id must parse")
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
        state.set_producer(&key, producer(22));
        let instance = |id| ParticipantInstanceKey {
            robot: robot.clone(),
            participant: "mission".to_string(),
            producer: producer(id),
        };
        state.record_instance_presence(instance(11), true);
        assert_eq!(state.process_state(&key), Some(ProcessState::Starting));
        state.record_instance_presence(instance(22), true);
        assert_eq!(state.process_state(&key), Some(ProcessState::Ready));
        state.record_instance_presence(instance(22), false);
        assert_eq!(state.process_state(&key), Some(ProcessState::Ready));
    }

    #[tokio::test]
    async fn snapshots_are_full_revisioned_values() {
        let state = SupervisorState::new();
        let mut consumer = state.subscribe();
        state.upsert_process(
            ProcessKey::project("router"),
            ParticipantKind::Tool,
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
            state.supervisor_snapshot().revision
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
        let snapshot = state.supervisor_snapshot();
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
        assert!(
            failure.stderr_tail.unwrap().as_str().len()
                <= phoxal_cli_protocol::limits::MAX_PROCESS_STDERR_TAIL_BYTES
        );
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
    fn graph_recovery_fences_old_readiness_and_publishes_a_new_generation() {
        let state = SupervisorState::new();
        let robot = RobotKey::new("lab", "rover");
        let key = ProcessKey::robot(robot.clone(), "mission");
        state.upsert_process(
            key.clone(),
            ParticipantKind::Service,
            ProcessState::Ready,
            StartupRequirement::Required,
        );
        state.set_pid(&key, Some(42));
        state.set_producer(&key, producer(7));
        let revision = state.supervisor_snapshot().revision;

        assert_eq!(state.begin_recovery_epoch(&[(key.clone(), None)], &[]), 1);
        let recovered = state.supervisor_snapshot();
        assert!(recovered.revision > revision);
        assert_eq!(recovered.graph_generation, 1);
        assert_eq!(recovered.lifecycle, ProjectLifecycle::Starting);
        assert_eq!(
            recovered.processes[&key].status.actual,
            ProcessState::Starting
        );
        assert_eq!(recovered.processes[&key].status.pid, None);
        assert_eq!(recovered.processes[&key].status.producer, None);

        state.record_instance_presence(
            ParticipantInstanceKey {
                robot,
                participant: "mission".to_string(),
                producer: producer(7),
            },
            true,
        );
        assert_eq!(state.process_state(&key), Some(ProcessState::Starting));
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
        let snapshot = state.supervisor_snapshot();
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
