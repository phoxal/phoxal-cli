//! The authoritative execution snapshot, projected for the terminal.
//!
//! It is also the disconnection feed: the connection's first structured
//! terminal cause is the authoritative explanation, and a socket that still
//! exists proves nothing.

use std::sync::Arc;

use phoxal_cli_observation::{
    AttachmentEvent, ConnectionObservation, LocalRuntimes, ObservationSource, ProcessObservation,
    ProcessTable, SourceStatus, SupervisorObservation,
};
use phoxal_client::DisconnectReason;
use phoxal_client::supervisor::execution::Snapshot;

use super::FeedContext;

pub(crate) async fn run(context: FeedContext) {
    let mut snapshots = context.client.snapshots();
    context
        .health(ObservationSource::Supervisor, SourceStatus::Live)
        .await;
    loop {
        let installed = snapshots.borrow_and_update().clone();
        if let Some(snapshot) = installed
            && publish(&context, &snapshot).await.is_err()
        {
            return;
        }
        tokio::select! {
            () = context.cancellation.cancelled() => return,
            reason = context.client.disconnected() => {
                // The first terminal cause latches for the connection. Tell the
                // UI before the event stream ends so it renders the real cause
                // rather than replacing it with a generic empty-stream error.
                announce_lost(&context, reason).await;
                return;
            }
            changed = snapshots.changed() => {
                if changed.is_err() {
                    announce_lost(&context, context.client.disconnected().await).await;
                    return;
                }
            }
        }
    }
}

async fn announce_lost(context: &FeedContext, reason: DisconnectReason) {
    let _ = context
        .events
        .send(AttachmentEvent::ConnectionChanged(lost_observation(reason)))
        .await;
}

fn lost_observation(reason: DisconnectReason) -> ConnectionObservation {
    ConnectionObservation::Lost {
        reason: reason.to_string().into(),
    }
}

async fn publish(context: &FeedContext, snapshot: &Snapshot) -> Result<(), ()> {
    let processes = expected_runtimes(snapshot, &context.local.read());
    context
        .events
        .send(AttachmentEvent::SupervisorChanged(Arc::new(observation(
            context, snapshot,
        ))))
        .await
        .map_err(|_| ())?;
    context
        .events
        .send(AttachmentEvent::ProcessesChanged {
            epoch: context.epoch,
            values: Arc::new(processes),
        })
        .await
        .map_err(|_| ())
}

/// Merge the authoritative snapshot with this client's own child facts.
///
/// The snapshot is the whole remote truth and it is thin on purpose: a runtime
/// is present or it is not. Everything that explains an absence - an exit
/// status, a log path - comes from `local`, and only a session that launched
/// the runtime has any.
fn expected_runtimes(snapshot: &Snapshot, local: &LocalRuntimes) -> ProcessTable {
    snapshot
        .processes
        .iter()
        .map(|row| {
            (
                row.participant.clone(),
                ProcessObservation {
                    row: row.clone(),
                    local: local.get(&row.participant).cloned(),
                },
            )
        })
        .collect()
}

fn observation(context: &FeedContext, snapshot: &Snapshot) -> SupervisorObservation {
    SupervisorObservation {
        revision: snapshot.revision,
        execution: context.epoch.execution,
        robot: context.client.connected().robot.clone(),
        project: context.project.clone(),
        lifecycle: snapshot.lifecycle,
    }
}

#[cfg(test)]
mod tests {
    use phoxal_cli_observation::{LocalRuntime, LocalRuntimeState};
    use phoxal_client::BusFault;
    use phoxal_client::supervisor::execution::{Lifecycle, Process, ProcessState};
    use phoxal_runtime_contract::identity::ParticipantId;
    use phoxal_runtime_contract::metadata::ParticipantKind;

    use super::*;

    fn brain() -> ParticipantId {
        ParticipantId::new("brain").expect("fixture participant")
    }

    fn absent_brain() -> Snapshot {
        Snapshot {
            revision: 1,
            lifecycle: Lifecycle::Starting,
            processes: vec![Process {
                participant: brain(),
                kind: ParticipantKind::Brain,
                state: ProcessState::Absent,
                producer: None,
            }],
        }
    }

    /// A session that launched the runtime carries the answer the snapshot
    /// cannot give: why an absent row is absent, and where to read the rest.
    #[test]
    fn a_launched_runtimes_own_exit_is_merged_onto_its_absent_row() {
        let local = LocalRuntimes::from([(
            brain(),
            LocalRuntime {
                state: LocalRuntimeState::Exited {
                    status: "exit status: 1".to_string(),
                },
                log: std::path::PathBuf::from("/tmp/rover/.phoxal/run/log/brain.log"),
            },
        )]);

        let table = expected_runtimes(&absent_brain(), &local);
        let observed = table[&brain()]
            .local
            .as_ref()
            .expect("a launched runtime carries its own child facts");
        assert_eq!(observed.state.label(), "exit status: 1");
        assert_eq!(
            observed.log,
            std::path::PathBuf::from("/tmp/rover/.phoxal/run/log/brain.log")
        );

        // An attachment to somebody else's execution has none of this, and the
        // row is still a complete row.
        let attached = expected_runtimes(&absent_brain(), &LocalRuntimes::new());
        assert!(attached[&brain()].local.is_none());
        assert_eq!(attached[&brain()].row.state, ProcessState::Absent);
    }

    #[test]
    fn every_disconnect_reason_reaches_the_tui_unchanged() {
        for reason in [
            DisconnectReason::ConnectionClosed,
            DisconnectReason::SupervisorIdentityLost,
            DisconnectReason::SnapshotStreamFailed {
                detail: "snapshot subscriber closed".to_string(),
            },
            DisconnectReason::TransportFault {
                fault: BusFault::WorkerExited {
                    worker: "outbound-drain".to_string(),
                },
            },
            DisconnectReason::LifecycleEnded,
        ] {
            let expected = reason.to_string();
            assert_eq!(
                lost_observation(reason),
                ConnectionObservation::Lost {
                    reason: expected.into()
                }
            );
        }
    }
}
