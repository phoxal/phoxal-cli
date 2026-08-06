//! The daemon's authoritative execution state.
//!
//! Two authorities sit behind one value here, deliberately:
//!
//! - the internal [`SupervisorState`] store, which the process machinery
//!   writes to on every spawn, exit, and liveliness token;
//! - the daemon's own facts - the robot identity, the execution mode, the
//!   startup sequence, and the typed daemon failure - which no process
//!   observation can produce.
//!
//! Every change to either republishes one complete snapshot at the next
//! revision. `revision` is monotonic within the execution and is assigned here,
//! at the single publication point, so a client that keeps the highest revision
//! it has seen can never install an older document over a newer one.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use phoxal_supervisor_api::{
    DaemonFailure, DaemonFailureReason, Detail, Snapshot, StartupStep, StartupStepKind,
    StartupStepState,
};
use tokio::sync::watch;

use super::projection::{ExecutionFacts, project};
use crate::SupervisorState;

/// The daemon's startup sequence, in the order `phoxald` performs it.
const STARTUP_SEQUENCE: [StartupStepKind; 5] = [
    StartupStepKind::Bundle,
    StartupStepKind::Requirements,
    StartupStepKind::Binaries,
    StartupStepKind::Router,
    StartupStepKind::Participants,
];

/// Shared handle to one execution's state. Cloning shares it.
#[derive(Clone)]
pub(crate) struct ExecutionState {
    board: SupervisorState,
    inner: Arc<Inner>,
}

struct Inner {
    facts: Mutex<ExecutionFacts>,
    startup: Mutex<Vec<StartupStep>>,
    started: Mutex<Vec<(StartupStepKind, Instant)>>,
    failure: Mutex<Option<DaemonFailure>>,
    revision: AtomicU64,
    published: watch::Sender<Snapshot>,
}

impl ExecutionState {
    /// Start an execution's state from the facts known before the bundle is
    /// even read: nothing has been validated yet, so the identity and mode are
    /// placeholders the loader replaces through [`Self::identify`].
    pub(crate) fn new(board: SupervisorState, facts: ExecutionFacts) -> Self {
        let startup: Vec<_> = STARTUP_SEQUENCE
            .into_iter()
            .map(|kind| StartupStep {
                kind,
                state: StartupStepState::Pending,
                detail: None,
                elapsed_ms: None,
            })
            .collect();
        let initial = project(0, &facts, &startup, None, &board.supervisor_snapshot());
        let (published, _) = watch::channel(initial);
        Self {
            board,
            inner: Arc::new(Inner {
                facts: Mutex::new(facts),
                startup: Mutex::new(startup),
                started: Mutex::new(Vec::new()),
                failure: Mutex::new(None),
                revision: AtomicU64::new(0),
                published,
            }),
        }
    }

    pub(crate) fn board(&self) -> &SupervisorState {
        &self.board
    }

    /// The most recently published snapshot. This is what the `current` query
    /// answers with.
    pub(crate) fn snapshot(&self) -> Snapshot {
        self.inner.published.borrow().clone()
    }

    /// Observe every published snapshot. Used by the diagnostic publisher.
    pub(crate) fn subscribe(&self) -> watch::Receiver<Snapshot> {
        self.inner.published.subscribe()
    }

    /// The stable authored robot identity, as the connect reply advertises it.
    pub(crate) fn robot(&self) -> phoxal_supervisor_api::RobotIdentity {
        self.facts_lock().robot.clone()
    }

    /// The finalized manifest's `clock:` field, passed through untouched.
    pub(crate) fn mode(&self) -> phoxal_supervisor_api::ExecutionMode {
        self.facts_lock().mode
    }

    /// The selected process set, for resolving a command's target.
    pub(crate) fn roster(&self) -> super::roster::Roster {
        self.facts_lock().roster.clone()
    }

    /// Adopt the identity and mode the finalized manifest declares, together
    /// with the selected process set derived from it.
    pub(crate) fn identify(&self, facts: ExecutionFacts) {
        *self.facts_lock() = facts;
        self.republish();
    }

    pub(crate) fn step_active(&self, kind: StartupStepKind) {
        {
            let mut startup = self.startup_lock();
            let Some(step) = startup.iter_mut().find(|step| step.kind == kind) else {
                return;
            };
            if step.state == StartupStepState::Active {
                return;
            }
            step.state = StartupStepState::Active;
            step.elapsed_ms = None;
            self.started_lock().push((kind, Instant::now()));
        }
        self.republish();
    }

    pub(crate) fn step_detail(&self, kind: StartupStepKind, detail: impl AsRef<str>) {
        {
            let mut startup = self.startup_lock();
            let Some(step) = startup.iter_mut().find(|step| step.kind == kind) else {
                return;
            };
            step.detail = Some(Detail::new(detail));
        }
        self.republish();
    }

    pub(crate) fn step_done(&self, kind: StartupStepKind) {
        self.finish_step(kind, StartupStepState::Done, None);
    }

    pub(crate) fn step_failed(&self, kind: StartupStepKind, detail: impl AsRef<str>) {
        self.finish_step(
            kind,
            StartupStepState::Failed,
            Some(Detail::new(detail.as_ref())),
        );
    }

    fn finish_step(&self, kind: StartupStepKind, state: StartupStepState, detail: Option<Detail>) {
        {
            let elapsed = {
                let mut started = self.started_lock();
                let position = started.iter().position(|(step, _)| *step == kind);
                position.map(|position| started.remove(position).1.elapsed())
            };
            let mut startup = self.startup_lock();
            let Some(step) = startup.iter_mut().find(|step| step.kind == kind) else {
                return;
            };
            if step.state == state {
                return;
            }
            step.state = state;
            if let Some(detail) = detail {
                step.detail = Some(detail);
            }
            step.elapsed_ms = elapsed
                .map(|elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
                .or(step.elapsed_ms);
        }
        self.republish();
    }

    /// Record why the execution failed, as a typed reason plus its evidence.
    ///
    /// First cause wins, exactly as the store's own `fail` does: an unwinding
    /// daemon hits several failure sites, and only the first one is the actual
    /// cause. The store is failed in the same call, so lifecycle and reason
    /// can never be observed apart.
    pub(crate) fn fail(&self, reason: DaemonFailureReason, detail: impl AsRef<str>) {
        let detail = detail.as_ref();
        {
            let mut failure = self.failure_lock();
            if failure.is_none() {
                *failure = Some(DaemonFailure {
                    reason,
                    detail: Detail::new(detail),
                });
            }
        }
        // Publishes on its own, which the bridge turns into one republication.
        self.board.fail(detail);
        self.republish();
    }

    pub(crate) fn failure(&self) -> Option<DaemonFailure> {
        self.failure_lock().clone()
    }

    /// Rebuild and publish one complete snapshot at the next revision.
    pub(crate) fn republish(&self) {
        let revision = self.inner.revision.fetch_add(1, Ordering::SeqCst) + 1;
        let snapshot = project(
            revision,
            &self.facts_lock(),
            &self.startup_lock(),
            self.failure_lock().as_ref(),
            &self.board.supervisor_snapshot(),
        );
        self.inner.published.send_replace(snapshot);
    }

    /// Republish whenever the internal store changes, so a process spawning,
    /// exiting, or being proved ready reaches attached clients without every
    /// call site in the process machinery knowing this type exists.
    pub(crate) fn bridge_store_changes(&self) -> tokio::task::JoinHandle<()> {
        let state = self.clone();
        let mut changes = self.board.subscribe();
        tokio::spawn(async move {
            while changes.changed().await.is_ok() {
                changes.borrow_and_update();
                state.republish();
            }
        })
    }

    fn facts_lock(&self) -> std::sync::MutexGuard<'_, ExecutionFacts> {
        self.inner
            .facts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn startup_lock(&self) -> std::sync::MutexGuard<'_, Vec<StartupStep>> {
        self.inner
            .startup
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn started_lock(&self) -> std::sync::MutexGuard<'_, Vec<(StartupStepKind, Instant)>> {
        self.inner
            .started
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn failure_lock(&self) -> std::sync::MutexGuard<'_, Option<DaemonFailure>> {
        self.inner
            .failure
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use phoxal_supervisor_api::{ExecutionMode, Name, RobotIdentity};

    use super::*;
    use crate::daemon::roster::tests::roster;

    fn state() -> ExecutionState {
        ExecutionState::new(
            SupervisorState::new(),
            ExecutionFacts {
                robot: RobotIdentity {
                    id: Name::new("rover"),
                    namespace: Name::new("demo"),
                },
                mode: ExecutionMode::Real,
                roster: roster(),
            },
        )
    }

    #[test]
    fn the_startup_sequence_is_the_daemons_own_and_starts_entirely_pending() {
        let snapshot = state().snapshot();
        assert_eq!(
            snapshot
                .startup
                .iter()
                .map(|step| step.kind)
                .collect::<Vec<_>>(),
            STARTUP_SEQUENCE
        );
        assert!(
            snapshot
                .startup
                .iter()
                .all(|step| step.state == StartupStepState::Pending)
        );
        assert_eq!(snapshot.revision, 0);
    }

    #[test]
    fn every_change_publishes_a_strictly_higher_revision() {
        let state = state();
        let mut revisions = vec![state.snapshot().revision];
        state.step_active(StartupStepKind::Bundle);
        revisions.push(state.snapshot().revision);
        state.step_detail(StartupStepKind::Bundle, "robot.yaml");
        revisions.push(state.snapshot().revision);
        state.step_done(StartupStepKind::Bundle);
        revisions.push(state.snapshot().revision);
        assert!(
            revisions.windows(2).all(|pair| pair[1] > pair[0]),
            "{revisions:?}"
        );

        let done = state
            .snapshot()
            .startup
            .into_iter()
            .find(|step| step.kind == StartupStepKind::Bundle)
            .expect("the bundle step");
        assert_eq!(done.state, StartupStepState::Done);
        assert_eq!(done.detail.as_ref().map(Detail::as_str), Some("robot.yaml"));
        assert!(done.elapsed_ms.is_some(), "a finished step is timed");
    }

    #[test]
    fn the_first_daemon_failure_wins_and_reaches_the_snapshot_with_the_lifecycle() {
        let state = state();
        state.fail(DaemonFailureReason::RouterLost, "the router went away");
        state.fail(DaemonFailureReason::Internal, "and then everything else");

        let snapshot = state.snapshot();
        let failure = snapshot.failure.expect("the failure is published");
        assert_eq!(failure.reason, DaemonFailureReason::RouterLost);
        assert_eq!(failure.detail.as_str(), "the router went away");
        assert_eq!(
            snapshot.lifecycle,
            phoxal_supervisor_api::Lifecycle::Failed,
            "the store's lifecycle and the typed reason are one update"
        );
    }

    #[tokio::test]
    async fn a_store_change_republishes_without_the_process_machinery_knowing_this_type() {
        use phoxal_cli_core::runtime::{ParticipantKind, ProcessState, StartupRequirement};

        let state = state();
        let bridge = state.bridge_store_changes();
        let mut published = state.subscribe();
        let brain = state
            .snapshot()
            .processes
            .first()
            .expect("a selected process")
            .key
            .clone();

        let core = roster()
            .resolve(&brain)
            .expect("the roster row")
            .core
            .clone();
        state.board().upsert_process(
            core.clone(),
            ParticipantKind::Brain,
            ProcessState::Ready,
            StartupRequirement::Required,
        );

        // Republication is asynchronous, so wait for the value rather than
        // assuming the bridge has already run.
        loop {
            published.changed().await.expect("the publisher stays open");
            let snapshot = published.borrow_and_update().clone();
            if snapshot
                .processes
                .iter()
                .any(|process| process.state == phoxal_supervisor_api::ProcessState::Ready)
            {
                break;
            }
        }
        bridge.abort();
    }
}
