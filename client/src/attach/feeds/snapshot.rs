//! The authoritative execution snapshot, projected for the terminal.
//!
//! It is also the disconnection feed: losing the supervisor's identity
//! liveliness token is what "the execution is gone" means, and a socket that
//! still exists proves nothing.

use std::sync::Arc;

use phoxal_cli_observation::{
    AttachmentEvent, ConnectionObservation, SourceStatus, SupervisorObservation,
};
use phoxal_supervisor_api::Snapshot;

use super::FeedContext;

pub(crate) async fn run(context: FeedContext) {
    let mut snapshots = context.attachment.snapshots();
    context.health("supervisor", SourceStatus::Live).await;
    loop {
        let installed = snapshots.borrow_and_update().clone();
        if let Some(snapshot) = installed
            && publish(&context, &snapshot).await.is_err()
        {
            return;
        }
        tokio::select! {
            () = context.cancellation.cancelled() => return,
            () = context.attachment.disconnected() => {
                // Token loss latches, so this is terminal for the attachment.
                // The UI is told before the event stream ends so it can render
                // the reason rather than an empty screen.
                let _ = context
                    .events
                    .send(AttachmentEvent::ConnectionChanged(
                        ConnectionObservation::Lost {
                            reason: "the supervisor's identity token was lost; the execution ended"
                                .into(),
                        },
                    ))
                    .await;
                return;
            }
            changed = snapshots.changed() => {
                if changed.is_err() {
                    return;
                }
            }
        }
    }
}

async fn publish(context: &FeedContext, snapshot: &Snapshot) -> Result<(), ()> {
    let processes = context.stores.processes.write().await.replace(snapshot);
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
        robot: snapshot.robot.clone(),
        mode: snapshot.mode,
        project: context.project.clone(),
        lifecycle: snapshot.lifecycle,
        startup: snapshot.startup.clone(),
        failure: snapshot.failure.clone(),
    }
}
