//! Bounded typed ingress mailbox for the synchronous terminal loop.

use std::collections::VecDeque;

use phoxal_cli_observation::{AttachmentEpoch, AttachmentEvent};

use super::message::SessionInput;

const PENDING_CAPACITY: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum UserEvent {}

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
    StopSessionCompletion = 9,
    OwnedSupervisorExit = 10,
}

const SLOT_COUNT: usize = Slot::OwnedSupervisorExit as usize + 1;

pub(crate) struct PendingInputs {
    epoch: Option<AttachmentEpoch>,
    epoch_pending: bool,
    slots: [Option<SessionInput>; SLOT_COUNT],
    diagnostics: VecDeque<String>,
    controls: VecDeque<SessionInput>,
}

impl Default for PendingInputs {
    fn default() -> Self {
        Self {
            epoch: None,
            epoch_pending: false,
            slots: std::array::from_fn(|_| None),
            diagnostics: VecDeque::new(),
            controls: VecDeque::new(),
        }
    }
}

impl PendingInputs {
    pub(crate) fn push(&mut self, input: SessionInput) {
        match input {
            SessionInput::Client(AttachmentEvent::EpochChanged(epoch)) => {
                // A new epoch replaces every projection except the connection
                // observation, which is about this attachment rather than about
                // the execution it now describes. The supervisor slot is not
                // carried over: a snapshot has no terminal value left to
                // preserve, so keeping the old one would only render the
                // previous execution's rows under the new epoch.
                let connection = self.slots[Slot::Connection as usize].take();
                self.epoch = Some(epoch);
                self.epoch_pending = true;
                self.slots.fill(None);
                self.slots[Slot::Connection as usize] = connection;
            }
            SessionInput::Diagnostic(message) => {
                if self.diagnostics.len() == PENDING_CAPACITY {
                    self.diagnostics.pop_front();
                }
                self.diagnostics.push_back(message);
            }
            input @ (SessionInput::Interrupt | SessionInput::Terminate) => {
                if matches!(input, SessionInput::Terminate)
                    && self
                        .controls
                        .iter()
                        .any(|pending| matches!(pending, SessionInput::Terminate))
                {
                    return;
                }
                if self.controls.len() == PENDING_CAPACITY {
                    let Some(oldest_interrupt) = self
                        .controls
                        .iter()
                        .position(|pending| matches!(pending, SessionInput::Interrupt))
                    else {
                        return;
                    };
                    if self.controls.remove(oldest_interrupt).is_none() {
                        return;
                    }
                }
                self.controls.push_back(input);
            }
            input => {
                let Some(slot) = slot_for(&input) else {
                    return;
                };
                if input_epoch(&input).is_some_and(|epoch| Some(epoch) != self.epoch) {
                    return;
                }
                self.slots[slot as usize] = Some(input);
            }
        }
    }

    pub(crate) fn drain(&mut self) -> Vec<SessionInput> {
        let mut drained = Vec::with_capacity(
            usize::from(self.epoch_pending)
                + SLOT_COUNT
                + self.diagnostics.len()
                + self.controls.len(),
        );
        if self.epoch_pending {
            if let Some(epoch) = self.epoch {
                drained.push(SessionInput::Client(AttachmentEvent::EpochChanged(epoch)));
            }
            self.epoch_pending = false;
        }
        for slot in &mut self.slots {
            if let Some(input) = slot.take() {
                drained.push(input);
            }
        }
        drained.extend(self.diagnostics.drain(..).map(SessionInput::Diagnostic));
        drained.extend(self.controls.drain(..));
        drained
    }
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
        SessionInput::SessionStopped | SessionInput::StopSessionFailed(_) => {
            Some(Slot::StopSessionCompletion)
        }
        SessionInput::OwnedSupervisorStopped | SessionInput::OwnedSupervisorFailed(_) => {
            Some(Slot::OwnedSupervisorExit)
        }
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

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use phoxal_cli_observation::{
        AttachmentEpoch, AttachmentEvent, LogWindow, QueryToken, StoreChanged, StoreRevision,
        SupervisorObservation,
    };
    use phoxal_client::supervisor::execution::Lifecycle;
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
    fn repeated_invalidations_share_one_bounded_slot() {
        let mut pending = PendingInputs::default();
        pending.push(SessionInput::Client(AttachmentEvent::EpochChanged(
            changed_epoch(),
        )));
        pending.drain();
        pending.push(changed(1));
        for revision in 2..=1_000 {
            pending.push(changed(revision));
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
        let mut pending = PendingInputs::default();
        for index in 0..(PENDING_CAPACITY * 2) {
            pending.push(SessionInput::Diagnostic(index.to_string()));
        }
        assert_eq!(pending.drain().len(), PENDING_CAPACITY);
    }

    #[test]
    fn a_query_reply_has_a_fixed_slot_and_is_never_dropped() {
        let mut pending = PendingInputs::default();
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
        pending.push(reply);
        let drained = pending.drain();
        assert_eq!(drained.len(), PENDING_CAPACITY + 2);
        assert!(drained.iter().any(
            |input| matches!(input, SessionInput::Logs(window) if window.token == QueryToken(7))
        ));
    }

    #[test]
    fn epoch_change_is_a_barrier_and_purges_old_invalidations() {
        let mut pending = PendingInputs::default();
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
        let mut pending = PendingInputs::default();
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
        let mut pending = PendingInputs::default();
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

    /// A burst of diagnostics must never crowd out the one projection the
    /// operator is actually watching: the latest snapshot always survives,
    /// whatever it says.
    #[test]
    fn the_latest_supervisor_observation_survives_diagnostic_saturation() {
        let mut pending = PendingInputs::default();
        for index in 0..(PENDING_CAPACITY * 2) {
            pending.push(SessionInput::Diagnostic(index.to_string()));
        }
        pending.push(SessionInput::Client(AttachmentEvent::SupervisorChanged(
            Arc::new(SupervisorObservation {
                revision: 1,
                execution: ExecutionId::mint(),
                robot: RobotId::new("rover").expect("fixture robot id"),
                project: "project".into(),
                lifecycle: Lifecycle::Degraded,
            }),
        )));
        assert!(pending.drain().iter().any(|input| matches!(
            input,
            SessionInput::Client(AttachmentEvent::SupervisorChanged(supervisor))
                if supervisor.lifecycle == Lifecycle::Degraded
        )));
    }

    #[test]
    fn stop_completion_survives_diagnostic_saturation() {
        let mut pending = PendingInputs::default();
        for index in 0..(PENDING_CAPACITY * 2) {
            pending.push(SessionInput::Diagnostic(index.to_string()));
        }
        pending.push(SessionInput::StopSessionFailed("busy".to_string()));

        assert!(pending.drain().iter().any(|input| matches!(
            input,
            SessionInput::StopSessionFailed(reason) if reason == "busy"
        )));
    }

    #[test]
    fn interrupts_and_termination_are_preserved_in_order() {
        let mut pending = PendingInputs::default();
        pending.push(SessionInput::Interrupt);
        pending.push(SessionInput::Interrupt);
        pending.push(SessionInput::Terminate);

        assert_eq!(
            pending.drain(),
            vec![
                SessionInput::Interrupt,
                SessionInput::Interrupt,
                SessionInput::Terminate,
            ]
        );
    }

    #[test]
    fn termination_survives_control_saturation() {
        let mut pending = PendingInputs::default();
        pending.push(SessionInput::Terminate);
        for _ in 0..(PENDING_CAPACITY * 2) {
            pending.push(SessionInput::Interrupt);
        }

        let drained = pending.drain();
        assert_eq!(drained.len(), PENDING_CAPACITY);
        assert!(drained.contains(&SessionInput::Terminate));
    }

    fn changed_epoch() -> AttachmentEpoch {
        AttachmentEpoch::new(
            ExecutionId::parse(&"1".repeat(ExecutionId::LEN)).expect("fixed execution id"),
        )
    }
}
