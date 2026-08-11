//! Bounded typed ingress behind tui-realm's equality-constrained user event.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, PoisonError};

use phoxal_cli_observation::{AttachmentEpoch, AttachmentEvent};
use tokio::sync::mpsc;
use tuirealm::event::Event;
use tuirealm::listener::{PollAsync, PortResult};

use super::message::SessionInput;

const PENDING_CAPACITY: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum UserEvent {
    Wake,
}

#[derive(Debug, Clone, Copy)]
enum Slot {
    Connection = 0,
    Supervisor = 1,
    Processes = 2,
    Input = 3,
    Health = 4,
    LogsChanged = 5,
    RuntimesChanged = 6,
    LogsReply = 7,
    RuntimesReply = 8,
}

const SLOT_COUNT: usize = Slot::RuntimesReply as usize + 1;

struct Mailbox {
    epoch: Option<AttachmentEpoch>,
    epoch_pending: bool,
    slots: [Option<SessionInput>; SLOT_COUNT],
    diagnostics: VecDeque<String>,
    terminate: bool,
    wake_pending: bool,
}

impl Default for Mailbox {
    fn default() -> Self {
        Self {
            epoch: None,
            epoch_pending: false,
            slots: std::array::from_fn(|_| None),
            diagnostics: VecDeque::new(),
            terminate: false,
            wake_pending: false,
        }
    }
}

#[derive(Default)]
pub(crate) struct PendingInputs {
    state: Mutex<Mailbox>,
}

impl PendingInputs {
    fn push(&self, input: SessionInput) -> bool {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let accepted = match input {
            SessionInput::Client(AttachmentEvent::EpochChanged(epoch)) => {
                let connection = state.slots[Slot::Connection as usize].take();
                let terminal_supervisor = state.slots[Slot::Supervisor as usize]
                    .take()
                    .filter(is_terminal_supervisor);
                state.epoch = Some(epoch);
                state.epoch_pending = true;
                state.slots.fill(None);
                state.slots[Slot::Connection as usize] = connection;
                state.slots[Slot::Supervisor as usize] = terminal_supervisor;
                true
            }
            SessionInput::Diagnostic(message) => {
                if state.diagnostics.len() == PENDING_CAPACITY {
                    state.diagnostics.pop_front();
                }
                state.diagnostics.push_back(message);
                true
            }
            SessionInput::Terminate => {
                state.terminate = true;
                true
            }
            input => slot_for(&input).is_some_and(|slot| {
                if input_epoch(&input).is_some_and(|epoch| Some(epoch) != state.epoch) {
                    return false;
                }
                state.slots[slot as usize] = Some(input);
                true
            }),
        };
        if !accepted || state.wake_pending {
            return false;
        }
        state.wake_pending = true;
        true
    }

    pub(crate) fn drain(&self) -> Vec<SessionInput> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let mut drained = Vec::with_capacity(
            usize::from(state.epoch_pending)
                + SLOT_COUNT
                + state.diagnostics.len()
                + usize::from(state.terminate),
        );
        if state.epoch_pending {
            if let Some(epoch) = state.epoch {
                drained.push(SessionInput::Client(AttachmentEvent::EpochChanged(epoch)));
            }
            state.epoch_pending = false;
        }
        for slot in &mut state.slots {
            if let Some(input) = slot.take() {
                drained.push(input);
            }
        }
        drained.extend(state.diagnostics.drain(..).map(SessionInput::Diagnostic));
        if std::mem::take(&mut state.terminate) {
            drained.push(SessionInput::Terminate);
        }
        state.wake_pending = false;
        drained
    }
}

fn is_terminal_supervisor(input: &SessionInput) -> bool {
    matches!(
        input,
        SessionInput::Client(AttachmentEvent::SupervisorChanged(supervisor))
            if matches!(
                supervisor.lifecycle,
                phoxal_client::supervisor::snapshot::Lifecycle::Stopped
                    | phoxal_client::supervisor::snapshot::Lifecycle::Failed
            )
    )
}

fn slot_for(input: &SessionInput) -> Option<Slot> {
    match input {
        SessionInput::Client(AttachmentEvent::ConnectionChanged(_)) => Some(Slot::Connection),
        SessionInput::Client(AttachmentEvent::SupervisorChanged(_)) => Some(Slot::Supervisor),
        SessionInput::Client(AttachmentEvent::ProcessesChanged { .. }) => Some(Slot::Processes),
        SessionInput::Client(AttachmentEvent::InputChanged { .. }) => Some(Slot::Input),
        SessionInput::Client(AttachmentEvent::SourceHealthChanged { .. }) => Some(Slot::Health),
        SessionInput::Client(AttachmentEvent::LogsChanged(_)) => Some(Slot::LogsChanged),
        SessionInput::Client(AttachmentEvent::RuntimesChanged(_)) => Some(Slot::RuntimesChanged),
        SessionInput::Logs(_) => Some(Slot::LogsReply),
        SessionInput::Runtimes(_) => Some(Slot::RuntimesReply),
        SessionInput::Client(AttachmentEvent::EpochChanged(_))
        | SessionInput::Diagnostic(_)
        | SessionInput::Interrupt
        | SessionInput::Terminate => None,
    }
}

fn input_epoch(input: &SessionInput) -> Option<AttachmentEpoch> {
    // The reducer permits only one in-flight read per store. A fixed reply slot
    // therefore represents exactly one (epoch, token) window; an epoch change
    // clears it before the reducer can issue the next token.
    match input {
        SessionInput::Client(
            AttachmentEvent::ProcessesChanged { epoch, .. }
            | AttachmentEvent::InputChanged { epoch, .. }
            | AttachmentEvent::SourceHealthChanged { epoch, .. },
        ) => Some(*epoch),
        SessionInput::Client(
            AttachmentEvent::LogsChanged(changed) | AttachmentEvent::RuntimesChanged(changed),
        ) => Some(changed.epoch),
        SessionInput::Logs(window) => Some(window.epoch),
        SessionInput::Runtimes(window) => Some(window.epoch),
        _ => None,
    }
}

pub(crate) struct InputPort {
    receiver: mpsc::Receiver<SessionInput>,
    pending: Arc<PendingInputs>,
    closed: bool,
}

impl InputPort {
    #[must_use]
    pub(crate) fn new(receiver: mpsc::Receiver<SessionInput>, pending: Arc<PendingInputs>) -> Self {
        Self {
            receiver,
            pending,
            closed: false,
        }
    }
}

#[tuirealm::async_trait]
impl PollAsync<UserEvent> for InputPort {
    async fn poll(&mut self) -> PortResult<Option<Event<UserEvent>>> {
        if self.closed {
            std::future::pending::<()>().await;
            return Ok(None);
        }
        let Some(first) = self.receiver.recv().await else {
            self.closed = true;
            let wake = self.pending.push(SessionInput::Terminate);
            return Ok(wake.then_some(Event::User(UserEvent::Wake)));
        };
        let mut wake = self.pending.push(first);
        while let Ok(input) = self.receiver.try_recv() {
            wake |= self.pending.push(input);
        }
        Ok(wake.then_some(Event::User(UserEvent::Wake)))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use phoxal_cli_observation::{
        AttachmentEpoch, AttachmentEvent, LogWindow, QueryToken, StoreChanged, StoreRevision,
        SupervisorObservation,
    };
    use phoxal_client::supervisor::snapshot::Lifecycle;
    use phoxal_runtime_contract::clock::Clock;
    use phoxal_runtime_contract::identity::ExecutionId;
    use phoxal_runtime_contract::identity::RobotId;

    use super::*;

    fn changed(revision: u64) -> SessionInput {
        SessionInput::Client(AttachmentEvent::LogsChanged(StoreChanged {
            epoch: changed_epoch(),
            revision: StoreRevision(revision),
        }))
    }

    #[test]
    fn repeated_invalidations_share_one_bounded_slot_and_one_wake() {
        let pending = PendingInputs::default();
        pending.push(SessionInput::Client(AttachmentEvent::EpochChanged(
            changed_epoch(),
        )));
        pending.drain();
        assert!(pending.push(changed(1)));
        for revision in 2..=1_000 {
            assert!(!pending.push(changed(revision)));
        }
        let drained = pending.drain();
        assert_eq!(drained.len(), 1);
        let SessionInput::Client(AttachmentEvent::LogsChanged(changed)) = &drained[0] else {
            panic!("expected coalesced logs invalidation");
        };
        assert_eq!(changed.revision, StoreRevision(1_000));
    }

    #[test]
    fn pending_queue_never_exceeds_capacity() {
        let pending = PendingInputs::default();
        for index in 0..(PENDING_CAPACITY * 2) {
            pending.push(SessionInput::Diagnostic(index.to_string()));
        }
        assert_eq!(pending.drain().len(), PENDING_CAPACITY);
    }

    #[test]
    fn a_query_reply_has_a_fixed_slot_and_is_never_dropped() {
        let pending = PendingInputs::default();
        pending.push(SessionInput::Client(AttachmentEvent::EpochChanged(
            changed_epoch(),
        )));
        for index in 0..PENDING_CAPACITY {
            pending.push(SessionInput::Diagnostic(index.to_string()));
        }
        let reply = SessionInput::Logs(LogWindow {
            epoch: changed_epoch(),
            revision: StoreRevision(1),
            token: QueryToken(7),
            rows: Arc::from([]),
        });
        assert!(!pending.push(reply));
        let drained = pending.drain();
        assert_eq!(drained.len(), PENDING_CAPACITY + 2);
        assert!(drained.iter().any(
            |input| matches!(input, SessionInput::Logs(window) if window.token == QueryToken(7))
        ));
    }

    #[test]
    fn epoch_change_is_a_barrier_and_purges_old_invalidations() {
        let pending = PendingInputs::default();
        let old = changed_epoch();
        // A new execution is a new attachment, so a different execution id is
        // exactly what an epoch change is.
        let new = AttachmentEpoch::new(ExecutionId::mint());
        pending.push(SessionInput::Client(AttachmentEvent::EpochChanged(old)));
        pending.push(changed(1));
        pending.push(SessionInput::Client(AttachmentEvent::EpochChanged(new)));
        pending.push(SessionInput::Client(AttachmentEvent::LogsChanged(
            StoreChanged {
                epoch: new,
                revision: StoreRevision(2),
            },
        )));

        let drained = pending.drain();
        assert!(matches!(
            drained.first(),
            Some(SessionInput::Client(AttachmentEvent::EpochChanged(epoch))) if *epoch == new
        ));
        assert_eq!(
            drained
                .iter()
                .filter(|input| matches!(
                    input,
                    SessionInput::Client(AttachmentEvent::LogsChanged(_))
                ))
                .count(),
            1
        );
        assert!(drained.iter().any(|input| matches!(
            input,
            SessionInput::Client(AttachmentEvent::LogsChanged(changed))
                if changed.epoch == new && changed.revision == StoreRevision(2)
        )));
    }

    #[test]
    fn stale_query_reply_after_epoch_change_is_discarded() {
        let pending = PendingInputs::default();
        let old = changed_epoch();
        // A new execution is a new attachment, so a different execution id is
        // exactly what an epoch change is.
        let new = AttachmentEpoch::new(ExecutionId::mint());
        pending.push(SessionInput::Client(AttachmentEvent::EpochChanged(new)));
        pending.push(SessionInput::Logs(LogWindow {
            epoch: old,
            revision: StoreRevision(1),
            token: QueryToken(7),
            rows: Arc::from([]),
        }));
        assert!(
            !pending
                .drain()
                .iter()
                .any(|input| matches!(input, SessionInput::Logs(_)))
        );
    }

    #[test]
    fn epoch_and_termination_survive_diagnostic_saturation() {
        let pending = PendingInputs::default();
        for index in 0..(PENDING_CAPACITY * 2) {
            pending.push(SessionInput::Diagnostic(index.to_string()));
        }
        let epoch = changed_epoch();
        pending.push(SessionInput::Client(AttachmentEvent::EpochChanged(epoch)));
        pending.push(SessionInput::Terminate);
        let drained = pending.drain();
        assert!(matches!(
            drained.first(),
            Some(SessionInput::Client(AttachmentEvent::EpochChanged(value))) if *value == epoch
        ));
        assert!(matches!(drained.last(), Some(SessionInput::Terminate)));
    }

    #[test]
    fn terminal_supervisor_observation_survives_diagnostic_saturation() {
        let pending = PendingInputs::default();
        for index in 0..(PENDING_CAPACITY * 2) {
            pending.push(SessionInput::Diagnostic(index.to_string()));
        }
        pending.push(SessionInput::Client(AttachmentEvent::SupervisorChanged(
            Arc::new(SupervisorObservation {
                revision: 1,
                execution: ExecutionId::mint(),
                robot: RobotId::new("rover").expect("fixture robot id"),
                clock: Clock::Real,
                project: "project".into(),
                lifecycle: Lifecycle::Stopped,
                startup: Vec::new(),
                failure: None,
            }),
        )));
        assert!(pending.drain().iter().any(|input| matches!(
            input,
            SessionInput::Client(AttachmentEvent::SupervisorChanged(supervisor))
                if supervisor.lifecycle == Lifecycle::Stopped
        )));
    }

    fn changed_epoch() -> AttachmentEpoch {
        AttachmentEpoch::new(
            ExecutionId::parse(&"1".repeat(ExecutionId::LEN)).expect("fixed execution id"),
        )
    }
}
