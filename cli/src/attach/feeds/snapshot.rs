//! The authoritative execution snapshot, projected for the terminal.
//!
//! It is also the disconnection feed: the connection's first structured
//! terminal cause is the authoritative explanation, and a socket that still
//! exists proves nothing.

use std::sync::Arc;

use phoxal_cli_observation::{
    AttachmentEvent, ConnectionObservation, ObservationSource, SourceStatus, SupervisorObservation,
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
    let processes = context
        .stores
        .processes
        .write()
        .await
        .replace(snapshot, &context.local.read());
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

fn observation(context: &FeedContext, snapshot: &Snapshot) -> SupervisorObservation {
    SupervisorObservation {
        revision: snapshot.revision,
        execution: context.epoch.execution,
        robot: context.client.connected().robot.clone(),
        project: context.project.clone(),
        lifecycle: snapshot.lifecycle,
        startup: snapshot.startup.clone(),
    }
}

#[cfg(test)]
mod tests {
    use phoxal_client::BusFault;

    use super::*;

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
