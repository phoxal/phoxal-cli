//! Bounded typed ingress behind tui-realm's equality-constrained user event.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use tokio::sync::mpsc;
use tuirealm::event::Event;
use tuirealm::listener::{PollAsync, PortResult};

use super::message::SessionInput;

const PENDING_CAPACITY: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum UserEvent {
    Wake,
}

#[derive(Default)]
pub(crate) struct PendingInputs {
    queue: Mutex<VecDeque<SessionInput>>,
    wake_pending: AtomicBool,
}

impl PendingInputs {
    fn push(&self, input: SessionInput) -> bool {
        let mut queue = self.queue.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(slot) = queue
            .iter_mut()
            .find(|queued| same_coalescible_input(queued, &input))
        {
            *slot = input;
        } else {
            if queue.len() == PENDING_CAPACITY {
                let Some(drop_index) = queue.iter().position(|queued| !is_guaranteed(queued))
                else {
                    debug_assert!(
                        false,
                        "bounded input queue cannot contain only query replies and termination"
                    );
                    return false;
                };
                queue.remove(drop_index);
            }
            queue.push_back(input);
        }
        !self.wake_pending.swap(true, Ordering::AcqRel)
    }

    pub(crate) fn drain(&self) -> Vec<SessionInput> {
        let mut queue = self.queue.lock().unwrap_or_else(PoisonError::into_inner);
        let drained = queue.drain(..).collect();
        self.wake_pending.store(false, Ordering::Release);
        drained
    }
}

fn same_coalescible_input(left: &SessionInput, right: &SessionInput) -> bool {
    use phoxal_cli_observation::AttachmentEvent;
    matches!(
        (left, right),
        (
            SessionInput::Client(AttachmentEvent::LogsChanged(_)),
            SessionInput::Client(AttachmentEvent::LogsChanged(_))
        ) | (
            SessionInput::Client(AttachmentEvent::BusChanged(_)),
            SessionInput::Client(AttachmentEvent::BusChanged(_))
        ) | (
            SessionInput::Client(AttachmentEvent::RuntimesChanged(_)),
            SessionInput::Client(AttachmentEvent::RuntimesChanged(_))
        ) | (
            SessionInput::Client(AttachmentEvent::FreshnessChanged(_)),
            SessionInput::Client(AttachmentEvent::FreshnessChanged(_))
        ) | (SessionInput::Terminate, SessionInput::Terminate)
    )
}

fn is_guaranteed(input: &SessionInput) -> bool {
    matches!(
        input,
        SessionInput::Logs(_)
            | SessionInput::Bus(_)
            | SessionInput::Runtimes(_)
            | SessionInput::Terminate
    )
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

    use phoxal_cli_core::identity::ExecutionId;
    use phoxal_cli_observation::{
        AttachmentEpoch, AttachmentEvent, LogWindow, QueryToken, StoreChanged, StoreRevision,
    };

    use super::*;

    fn changed(revision: u64) -> SessionInput {
        SessionInput::Client(AttachmentEvent::LogsChanged(StoreChanged {
            epoch: AttachmentEpoch {
                supervisor_generation: 1,
                execution_id: ExecutionId::parse(&"0".repeat(ExecutionId::LEN))
                    .expect("fixed execution id"),
                graph_generation: 1,
            },
            revision: StoreRevision(revision),
        }))
    }

    #[test]
    fn repeated_invalidations_share_one_bounded_slot_and_one_wake() {
        let pending = PendingInputs::default();
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
    fn a_query_reply_displaces_diagnostics_and_is_never_dropped() {
        let pending = PendingInputs::default();
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
        assert_eq!(drained.len(), PENDING_CAPACITY);
        assert!(drained.iter().any(
            |input| matches!(input, SessionInput::Logs(window) if window.token == QueryToken(7))
        ));
    }

    fn changed_epoch() -> AttachmentEpoch {
        AttachmentEpoch {
            supervisor_generation: 1,
            execution_id: ExecutionId::parse(&"0".repeat(ExecutionId::LEN))
                .expect("fixed execution id"),
            graph_generation: 1,
        }
    }
}
