use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime};

use phoxal_cli_core::identity::{ExecutionId, ProducerId};
use phoxal_cli_core::runtime::{
    BoundedString, ExitDescription, ParticipantInstanceKey, ParticipantKind, ProcessDescriptor,
    ProcessEntry, ProcessFailure, ProcessFailureKind, ProcessKey, ProcessState, ProjectLifecycle,
    StartupRequirement, StartupStatus, StartupStep, StartupStepKind, StartupStepState,
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
    startup_started: Arc<Mutex<HashMap<StartupStepKind, Instant>>>,
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
            startup_started: Arc::default(),
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
        let reason = BoundedString::with_max_bytes(
            reason,
            phoxal_cli_protocol::limits::MAX_SUPERVISOR_FAILURE_REASON_BYTES,
        )
        .as_str()
        .to_string();
        self.modify(|snapshot| {
            snapshot.lifecycle = ProjectLifecycle::Failed;
            if snapshot.failure.is_none() {
                snapshot.failure = Some(reason);
            }
        });
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
        });
    }

    pub fn set_router_endpoint(&self, endpoint: impl Into<String>) {
        self.modify(|snapshot| snapshot.router = bounded_text(&endpoint.into()));
    }

    pub fn set_simulation_info(&self, profile: impl Into<String>, world: impl Into<String>) {
        self.modify(|snapshot| {
            snapshot.simulation = Some(phoxal_cli_core::runtime::SimulationSessionInfo {
                profile: bounded_text(&profile.into()),
                world: bounded_text(&world.into()),
            });
        });
    }

    pub fn plan_startup_steps(&self) {
        let mut started = self
            .startup_started
            .lock()
            .expect("startup timing mutex poisoned");
        started.clear();
        started.insert(StartupStepKind::Project, Instant::now());
        drop(started);
        self.modify(|snapshot| {
            snapshot.startup = StartupStatus {
                steps: [
                    StartupStepKind::Project,
                    StartupStepKind::PrepareRuntime,
                    StartupStepKind::Infrastructure,
                    StartupStepKind::Graph,
                ]
                .into_iter()
                .map(|kind| StartupStep {
                    kind,
                    state: if kind == StartupStepKind::Project {
                        StartupStepState::Active
                    } else {
                        StartupStepState::Pending
                    },
                    detail: None,
                    elapsed_ms: None,
                })
                .collect(),
            };
        });
    }

    pub fn step_active(&self, kind: StartupStepKind) {
        let mut started = self
            .startup_started
            .lock()
            .expect("startup timing mutex poisoned");
        self.modify(|snapshot| {
            if let Some(step) = startup_step_mut(snapshot, kind) {
                if step.state == StartupStepState::Active {
                    return;
                }
                started.insert(kind, Instant::now());
                step.state = StartupStepState::Active;
                step.elapsed_ms = None;
            }
        });
    }

    pub fn step_detail(&self, kind: StartupStepKind, detail: impl AsRef<str>) {
        let detail = bounded_step_detail(detail.as_ref());
        self.modify(|snapshot| {
            if let Some(step) = startup_step_mut(snapshot, kind) {
                step.detail = Some(detail);
            }
        });
    }

    pub fn step_done(&self, kind: StartupStepKind) {
        let mut started = self
            .startup_started
            .lock()
            .expect("startup timing mutex poisoned");
        self.modify(|snapshot| {
            if let Some(step) = startup_step_mut(snapshot, kind) {
                if step.state == StartupStepState::Done {
                    return;
                }
                step.state = StartupStepState::Done;
                step.elapsed_ms = started.remove(&kind).map(|started| {
                    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
                });
            }
        });
    }

    pub fn step_failed(&self, kind: StartupStepKind, error: impl AsRef<str>) {
        let mut started = self
            .startup_started
            .lock()
            .expect("startup timing mutex poisoned");
        let detail = bounded_step_detail(error.as_ref());
        self.modify(|snapshot| {
            if let Some(step) = startup_step_mut(snapshot, kind) {
                if step.state == StartupStepState::Failed {
                    return;
                }
                step.state = StartupStepState::Failed;
                step.detail = Some(detail);
                step.elapsed_ms = started.remove(&kind).map(|started| {
                    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
                });
            }
        });
    }

    pub fn fail_active_step(&self, error: impl AsRef<str>) {
        let mut started = self
            .startup_started
            .lock()
            .expect("startup timing mutex poisoned");
        let detail = bounded_step_detail(error.as_ref());
        self.modify(|snapshot| {
            let Some(step) = snapshot
                .startup
                .steps
                .iter_mut()
                .find(|step| step.state == StartupStepState::Active)
            else {
                return;
            };
            step.state = StartupStepState::Failed;
            step.detail = Some(detail);
            step.elapsed_ms = started
                .remove(&step.kind)
                .map(|started| u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX));
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

fn startup_step_mut(
    snapshot: &mut SupervisorSnapshotV0,
    kind: StartupStepKind,
) -> Option<&mut StartupStep> {
    snapshot
        .startup
        .steps
        .iter_mut()
        .find(|step| step.kind == kind)
}

#[cfg(test)]
fn startup_step(snapshot: &SupervisorSnapshotV0, kind: StartupStepKind) -> Option<&StartupStep> {
    snapshot.startup.steps.iter().find(|step| step.kind == kind)
}

fn bounded_step_detail(value: &str) -> String {
    let maximum = phoxal_cli_protocol::limits::MAX_STEP_DETAIL_BYTES;
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
    fn startup_steps_publish_typed_state_detail_and_elapsed_time() {
        let state = SupervisorState::new();
        state.plan_startup_steps();
        let planned = state.supervisor_snapshot();
        assert_eq!(planned.startup.steps.len(), 4);
        assert_eq!(planned.startup.steps[0].kind, StartupStepKind::Project);
        assert_eq!(planned.startup.steps[0].state, StartupStepState::Active);
        assert!(
            planned.startup.steps[1..]
                .iter()
                .all(|step| step.state == StartupStepState::Pending)
        );

        state.step_detail(StartupStepKind::Project, "robot.yaml · framework 0.45.1");
        state.step_done(StartupStepKind::Project);
        state.step_active(StartupStepKind::PrepareRuntime);
        state.step_failed(
            StartupStepKind::PrepareRuntime,
            "x".repeat(phoxal_cli_protocol::limits::MAX_STEP_DETAIL_BYTES * 2),
        );

        let snapshot = state.supervisor_snapshot();
        let project = startup_step(&snapshot, StartupStepKind::Project).unwrap();
        assert_eq!(project.state, StartupStepState::Done);
        assert!(project.elapsed_ms.is_some());
        let prepare = startup_step(&snapshot, StartupStepKind::PrepareRuntime).unwrap();
        assert_eq!(prepare.state, StartupStepState::Failed);
        assert!(prepare.elapsed_ms.is_some());
        assert!(
            prepare.detail.as_ref().unwrap().len()
                <= phoxal_cli_protocol::limits::MAX_STEP_DETAIL_BYTES
        );
    }

    #[test]
    fn completed_startup_step_accepts_late_detail_and_may_be_reactivated() {
        let state = SupervisorState::new();
        state.plan_startup_steps();
        state.step_detail(StartupStepKind::Project, "initial");
        state.step_done(StartupStepKind::Project);
        state.step_detail(StartupStepKind::Project, "late");
        let snapshot = state.supervisor_snapshot();
        assert_eq!(
            startup_step(&snapshot, StartupStepKind::Project)
                .unwrap()
                .detail
                .as_deref(),
            Some("late")
        );

        state.step_active(StartupStepKind::Project);
        let snapshot = state.supervisor_snapshot();
        let project = startup_step(&snapshot, StartupStepKind::Project).unwrap();
        assert_eq!(project.state, StartupStepState::Active);
        assert_eq!(project.elapsed_ms, None);

        state.step_active(StartupStepKind::Project);
        state.step_done(StartupStepKind::Project);
        let first_elapsed = startup_step(&state.supervisor_snapshot(), StartupStepKind::Project)
            .unwrap()
            .elapsed_ms;
        state.step_done(StartupStepKind::Project);
        assert_eq!(
            startup_step(&state.supervisor_snapshot(), StartupStepKind::Project)
                .unwrap()
                .elapsed_ms,
            first_elapsed,
            "repeated terminal updates must not erase the original timing"
        );
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
        let snapshot = state.supervisor_snapshot();
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
        let snapshot = state.supervisor_snapshot();
        assert_eq!(snapshot.lifecycle, ProjectLifecycle::Failed);
        assert!(
            snapshot.failure.expect("reason recorded").len()
                <= phoxal_cli_protocol::limits::MAX_SUPERVISOR_FAILURE_REASON_BYTES
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
