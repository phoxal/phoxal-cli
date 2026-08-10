//! The daemon's authoritative execution state.
//!
//! Process observations and daemon lifecycle facts are projected through this
//! single publication authority. Every accepted change republishes one complete snapshot at the next
//! revision. `revision` is monotonic within the execution and is assigned here,
//! at the single publication point, so a client that keeps the highest revision
//! it has seen can never install an older document over a newer one.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use phoxal_api::supervisor::snapshot::{
    DaemonFailure, DaemonFailureReason, Detail, Snapshot, StartupStep, StartupStepKind,
    StartupStepState,
};
use tokio::sync::watch;

use super::projection::{ExecutionFacts, project};
use crate::state::store::SupervisorState;

/// The daemon's startup sequence, in the order `phoxald` performs it.
const STARTUP_SEQUENCE: [StartupStepKind; 3] = [
    StartupStepKind::Bundle,
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
    board: SupervisorState,
    data: Mutex<ExecutionData>,
    revision: AtomicU64,
    published: watch::Sender<Snapshot>,
    /// Held across "take the next revision" *and* "publish it", so two
    /// concurrent republications can never interleave into a watch channel
    /// that ends on the lower of the two revisions.
    publication: Mutex<()>,
}

struct ExecutionData {
    facts: ExecutionFacts,
    startup: Vec<StartupStep>,
    started: Vec<(StartupStepKind, Instant)>,
    failure: Option<DaemonFailure>,
}

impl ExecutionState {
    /// Start an execution from its persisted participant roster before any
    /// startup step has begun.
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
        let initial = project(0, &facts, &startup, None, &board.snapshot());
        let (published, _) = watch::channel(initial);
        let inner = Arc::new(Inner {
            board: board.clone(),
            data: Mutex::new(ExecutionData {
                facts,
                startup,
                started: Vec::new(),
                failure: None,
            }),
            revision: AtomicU64::new(0),
            published,
            publication: Mutex::new(()),
        });
        let weak = Arc::downgrade(&inner);
        board.publish_with(move |_| {
            if let Some(inner) = weak.upgrade() {
                Self::publish(&inner);
            }
        });
        Self { board, inner }
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

    /// The selected process set, for resolving a command's target.
    pub(crate) fn roster(&self) -> super::roster::Roster {
        self.data_lock().facts.roster.clone()
    }

    pub(crate) fn step_active(&self, kind: StartupStepKind) {
        {
            let mut data = self.data_lock();
            let Some(step) = data.startup.iter_mut().find(|step| step.kind == kind) else {
                return;
            };
            if step.state == StartupStepState::Active {
                return;
            }
            step.state = StartupStepState::Active;
            step.elapsed_ms = None;
            data.started.push((kind, Instant::now()));
        }
        self.republish();
    }

    pub(crate) fn step_detail(&self, kind: StartupStepKind, detail: impl AsRef<str>) {
        {
            let mut data = self.data_lock();
            let Some(step) = data.startup.iter_mut().find(|step| step.kind == kind) else {
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
            Some(detail.as_ref().to_string()),
        );
    }

    fn finish_step(&self, kind: StartupStepKind, state: StartupStepState, detail: Option<String>) {
        {
            let mut data = self.data_lock();
            let position = data.started.iter().position(|(step, _)| *step == kind);
            let elapsed = position.map(|position| data.started.remove(position).1.elapsed());
            let Some(step) = data.startup.iter_mut().find(|step| step.kind == kind) else {
                return;
            };
            if step.state == state {
                return;
            }
            step.state = state;
            if let Some(detail) = detail {
                step.detail = Some(Detail::new(detail));
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
            let mut data = self.data_lock();
            if data.failure.is_none() {
                data.failure = Some(DaemonFailure::new(reason, detail));
            }
        }
        // The store invokes the one publication callback after atomically
        // applying the lifecycle failure.
        self.board.fail(detail);
    }

    pub(crate) fn failure(&self) -> Option<DaemonFailure> {
        self.data_lock().failure.clone()
    }

    /// Rebuild and publish one complete snapshot at the next revision.
    ///
    /// Assigning the revision and installing the snapshot are one critical
    /// section. Split, two callers can take revisions 4 and 5 and then publish
    /// them in the other order, leaving the watch channel - and therefore every
    /// attached client's `current` answer - resting on revision 4 forever.
    pub(crate) fn republish(&self) {
        Self::publish(&self.inner);
    }

    fn publish(inner: &Inner) {
        let _publication = inner
            .publication
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Read the authoritative board only after publication is serialized.
        // Otherwise a delayed callback could assign a newer revision to an
        // older clone and make clients retain stale state.
        let board = inner.board.snapshot();
        let revision = inner.revision.fetch_add(1, Ordering::SeqCst) + 1;
        let data = inner
            .data
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let snapshot = project(
            revision,
            &data.facts,
            &data.startup,
            data.failure.as_ref(),
            &board,
        );
        inner.published.send_replace(snapshot);
    }

    fn data_lock(&self) -> std::sync::MutexGuard<'_, ExecutionData> {
        self.inner
            .data
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::roster::Roster;

    fn state() -> ExecutionState {
        ExecutionState::new(
            SupervisorState::new(),
            ExecutionFacts {
                roster: Roster::test_fixture(),
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
            phoxal_api::supervisor::snapshot::Lifecycle::Failed,
            "the store's lifecycle and the typed reason are one update"
        );
    }

    #[test]
    fn a_store_change_republishes_without_the_process_machinery_knowing_this_type() {
        use crate::model::{ParticipantKind, ProcessState, StartupRequirement};

        let state = state();
        let before = state.snapshot().revision;
        let brain = state
            .snapshot()
            .processes
            .first()
            .expect("a selected process")
            .participant
            .clone();

        let core = Roster::test_fixture()
            .resolve(&brain)
            .expect("the roster row")
            .key
            .clone();
        state.board().upsert_process(
            core.clone(),
            ParticipantKind::Brain,
            ProcessState::Ready,
            StartupRequirement::Required,
        );

        let snapshot = state.snapshot();
        assert!(snapshot.revision > before);
        assert!(
            snapshot
                .processes
                .iter()
                .any(|process| process.state
                    == phoxal_api::supervisor::snapshot::ProcessState::Ready)
        );
    }
}
