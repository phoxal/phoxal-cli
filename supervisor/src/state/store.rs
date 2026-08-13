use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::SystemTime;

use super::board::Board;
use crate::model::lifecycle::ProjectLifecycle;
use crate::model::process::{
    BoundedString, ExitDescription, ProcessEntry, ProcessFailure, ProcessFailureKind, ProcessKey,
    ProcessState, ProcessStatus,
};
use phoxal_runtime_contract::identity::ParticipantId;
use phoxal_runtime_contract::identity::ProducerId;

const DETAIL_MAX_BYTES: usize = 4 * 1024;
const STDERR_TAIL_MAX_BYTES: usize = 32 * 1024;

/// The daemon's authoritative process and lifecycle state.
///
/// It is typed on the internal [`Board`], never on a wire document: the
/// execution's identity, mode, startup sequence, and typed failure are the
/// daemon's own facts and live in `daemon::state`, and disposable attachment
/// projections, retained observations, logs, telemetry, and UI channels live
/// outside the daemon entirely.
#[derive(Clone, Default)]
pub(crate) struct SupervisorState {
    inner: Arc<Mutex<StoreData>>,
    on_change: Arc<OnceLock<ChangeCallback>>,
}

type ChangeCallback = Arc<dyn Fn(Board) + Send + Sync>;

#[derive(Debug, Default)]
struct StoreData {
    board: Board,
    exact_instances: ExactReadiness,
    captured_stderr: HashMap<ProcessKey, VecDeque<String>>,
}

#[derive(Debug, Default)]
struct ExactReadiness {
    producers: HashMap<ParticipantId, HashSet<ProducerId>>,
}

impl std::fmt::Debug for SupervisorState {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SupervisorState")
            .finish_non_exhaustive()
    }
}

impl SupervisorState {
    #[must_use]
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn publish_with(&self, callback: impl Fn(Board) + Send + Sync + 'static) {
        let _ = self.on_change.set(Arc::new(callback));
    }

    /// Consume a participant's liveliness token as startup proof, and as the
    /// one place the supervisor learns which producer this incarnation
    /// actually publishes under.
    pub(crate) fn record_instance_presence(
        &self,
        participant: ParticipantId,
        producer: ProducerId,
        present: bool,
    ) -> Option<String> {
        let mut data = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let producers = data
            .exact_instances
            .producers
            .entry(participant.clone())
            .or_default();
        if present {
            producers.insert(producer);
        } else {
            producers.remove(&producer);
        }
        let producer_count = producers.len();
        if producer_count == 0 {
            data.exact_instances.producers.remove(&participant);
        }
        let project_key = participant.clone().into();
        let entry = data.board.processes.get(&project_key)?;
        let state = entry.status.actual;
        let active_producer = entry.status.producer;
        let mut failure = None;
        if producer_count > 1 {
            let detail = format!(
                "participant {participant} has {producer_count} simultaneous Ready producers"
            );
            let stderr_tail = stderr_tail(&data, &project_key);
            if let Some(entry) = data.board.processes.get_mut(&project_key) {
                entry.status.actual = ProcessState::Failed;
                entry.status.last_failure = Some(ProcessFailure {
                    kind: ProcessFailureKind::ReadinessConflict,
                    occurred_at: SystemTime::now(),
                    exit: None,
                    detail: BoundedString::new(&detail),
                    stderr_tail,
                });
            }
            failure = Some(detail);
        } else if present && state == ProcessState::Starting {
            // The token is the first evidence this incarnation has a session,
            // so it carries both facts at once: which producer it publishes
            // under, and that it is ready.
            if let Some(entry) = data.board.processes.get_mut(&project_key) {
                entry.status.producer = Some(producer);
                entry.status.actual = ProcessState::Ready;
            }
        } else if !present && active_producer == Some(producer) && state == ProcessState::Ready {
            let detail = format!("participant {participant} lost its active Ready producer");
            let stderr_tail = stderr_tail(&data, &project_key);
            if let Some(entry) = data.board.processes.get_mut(&project_key) {
                entry.status.actual = ProcessState::Failed;
                entry.status.last_failure = Some(ProcessFailure {
                    kind: ProcessFailureKind::Exit,
                    occurred_at: SystemTime::now(),
                    exit: None,
                    detail: BoundedString::new(&detail),
                    stderr_tail,
                });
            }
            failure = Some(detail);
        }
        let snapshot = data.board.clone();
        drop(data);
        self.changed(snapshot);
        failure
    }

    pub(crate) fn upsert_process(&self, key: ProcessKey, state: ProcessState) {
        self.modify(|board| {
            board.processes.insert(
                key,
                ProcessEntry {
                    status: ProcessStatus {
                        actual: state,
                        ..Default::default()
                    },
                },
            );
        });
    }

    pub(crate) fn register_planned(&self, key: &ProcessKey) {
        if self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .board
            .processes
            .contains_key(key)
        {
            return;
        }
        self.upsert_process(key.clone(), ProcessState::Starting);
    }

    pub(crate) fn set_state(&self, key: impl Into<ProcessKey>, state: ProcessState) {
        let key = key.into();
        self.modify(|board| {
            if let Some(entry) = board.processes.get_mut(&key) {
                entry.status.actual = state;
                if matches!(state, ProcessState::Starting | ProcessState::Restarting) {
                    entry.status.pid = None;
                }
            }
        });
    }

    pub(crate) fn record_failure(
        &self,
        key: impl Into<ProcessKey>,
        kind: ProcessFailureKind,
        exit: Option<ExitDescription>,
        detail: impl Into<String>,
    ) {
        let key = key.into();
        let detail = detail.into();
        self.modify_data(|data| {
            let stderr_tail = stderr_tail(data, &key);
            let board = &mut data.board;
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

    pub(crate) fn record_restart(&self, key: impl Into<ProcessKey>) {
        let key = key.into();
        self.modify(|board| {
            if let Some(entry) = board.processes.get_mut(&key) {
                entry.status.restart_count_total =
                    entry.status.restart_count_total.saturating_add(1);
            }
        });
    }

    pub(crate) fn set_pid(&self, key: impl Into<ProcessKey>, pid: Option<u32>) {
        let key = key.into();
        self.modify(|board| {
            if let Some(entry) = board.processes.get_mut(&key) {
                entry.status.pid = pid;
            }
        });
    }

    /// Retain bounded stderr solely as supervisor-owned failure evidence.
    pub(crate) fn record_captured_stderr(&self, key: &ProcessKey, line: &str) {
        const MAX_LINES: usize = 8;
        let line = BoundedString::with_max_bytes(line, STDERR_TAIL_MAX_BYTES)
            .as_str()
            .to_string();
        let mut data = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let lines = data.captured_stderr.entry(key.clone()).or_default();
        lines.push_back(line);
        while lines.len() > MAX_LINES {
            lines.pop_front();
        }
    }

    pub(crate) fn clear_captured_stderr(&self, key: &ProcessKey) {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .captured_stderr
            .remove(key);
    }

    /// Fence all readiness evidence before a new process incarnation starts.
    pub(crate) fn prepare_incarnation(&self, key: &ProcessKey) {
        self.modify_data(|data| {
            data.exact_instances.producers.remove(key.participant());
            if let Some(entry) = data.board.processes.get_mut(key) {
                entry.status.producer = None;
            }
        });
    }

    pub(crate) fn set_lifecycle(&self, lifecycle: ProjectLifecycle) {
        self.modify(|board| board.lifecycle = lifecycle);
    }

    /// Record the lifecycle as `Failed` together with the reason, in one
    /// atomic update so a subscriber can never observe `Failed` without the
    /// reason that caused it. This is the execution-level failure (a
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
    pub(crate) fn fail(&self, reason: &str) {
        let reason = BoundedString::with_max_bytes(reason, DETAIL_MAX_BYTES)
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
    pub(crate) fn process_state(&self, key: &ProcessKey) -> Option<ProcessState> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .board
            .processes
            .get(key)
            .map(|entry| entry.status.actual)
    }

    #[must_use]
    pub(crate) fn snapshot(&self) -> Board {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .board
            .clone()
    }

    fn modify(&self, update: impl FnOnce(&mut Board)) {
        self.modify_data(|data| update(&mut data.board));
    }

    fn modify_data(&self, update: impl FnOnce(&mut StoreData)) {
        let mut data = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        update(&mut data);
        let snapshot = data.board.clone();
        drop(data);
        self.changed(snapshot);
    }

    fn changed(&self, snapshot: Board) {
        if let Some(callback) = self.on_change.get() {
            callback(snapshot);
        }
    }
}

fn stderr_tail(data: &StoreData, key: &ProcessKey) -> Option<BoundedString> {
    data.captured_stderr
        .get(key)
        .map(|lines| lines.iter().cloned().collect::<Vec<_>>().join("\n"))
        .filter(|tail| !tail.is_empty())
        .map(|tail| BoundedString::with_max_bytes(tail, STDERR_TAIL_MAX_BYTES))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn producer(seed: u8) -> ProducerId {
        // A producer id is the publisher's session ZID: 1 to 32 lowercase hex
        // characters with no leading zero, so a seed renders bare.
        ProducerId::try_from((1_u128 << 124) | u128::from(seed))
            .expect("test producer id must parse")
    }

    fn key(id: &str) -> ProcessKey {
        ParticipantId::new(id).expect("fixture participant").into()
    }

    #[test]
    fn exact_producer_readiness_rejects_stale_holder() {
        let state = SupervisorState::new();
        let key = key("mission");
        state.upsert_process(key.clone(), ProcessState::Starting);
        let instance = ParticipantId::new("mission").expect("fixture participant");
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

        let failure = state
            .record_instance_presence(instance, producer(22), false)
            .expect("losing the active producer fails closed");
        assert!(failure.contains("lost its active Ready producer"));
        assert_eq!(state.process_state(&key), Some(ProcessState::Failed));
    }

    #[test]
    fn late_loss_from_prior_incarnation_cannot_revoke_new_readiness() {
        let state = SupervisorState::new();
        let key = key("mission");
        let participant = ParticipantId::new("mission").expect("fixture participant");
        let old_producer = producer(22);
        let new_producer = producer(23);
        state.upsert_process(key.clone(), ProcessState::Starting);
        state.record_instance_presence(participant.clone(), old_producer, true);

        state.prepare_incarnation(&key);
        state.set_state(key.clone(), ProcessState::Starting);
        state.record_instance_presence(participant.clone(), new_producer, true);
        assert_eq!(state.process_state(&key), Some(ProcessState::Ready));

        assert_eq!(
            state.record_instance_presence(participant, old_producer, false),
            None
        );
        let snapshot = state.snapshot();
        assert_eq!(snapshot.processes[&key].status.actual, ProcessState::Ready);
        assert_eq!(snapshot.processes[&key].status.producer, Some(new_producer));
    }

    #[test]
    fn every_store_mutation_invokes_the_publication_authority() {
        let state = SupervisorState::new();
        let publications = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let observed = Arc::clone(&publications);
        state.publish_with(move |_| {
            observed.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        });
        state.upsert_process(key("router"), ProcessState::Starting);
        state.set_lifecycle(ProjectLifecycle::Ready);
        assert_eq!(publications.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[test]
    fn failures_are_bounded_and_published() {
        let state = SupervisorState::new();
        let key = key("worker");
        state.upsert_process(key.clone(), ProcessState::Starting);
        state.record_instance_presence(
            ParticipantId::new("worker").expect("fixture participant"),
            producer(91),
            true,
        );
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
        assert!(failure.stderr_tail.unwrap().as_str().len() <= STDERR_TAIL_MAX_BYTES);
    }

    #[test]
    fn fail_sets_lifecycle_and_reason_in_one_atomic_update() {
        let state = SupervisorState::new();
        state.fail("catalog train floor not supported: 0.41.2 < 0.42.0");
        let snapshot = state.snapshot();
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
        assert!(snapshot.failure.expect("reason recorded").len() <= DETAIL_MAX_BYTES);
    }

    #[test]
    fn stderr_failure_evidence_is_scoped_to_one_process_incarnation() {
        let state = SupervisorState::new();
        let key = key("worker");
        state.record_captured_stderr(&key, "previous incarnation panic");
        state.clear_captured_stderr(&key);
        assert_eq!(
            stderr_tail(
                &state
                    .inner
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
                &key,
            ),
            None
        );
    }
}
