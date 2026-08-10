//! The robot's arbitrated motion state.
//!
//! This is the one feed that reads a robot-domain topic rather than the
//! supervisor contract. It opens on the attachment's own session, so its key
//! is already rooted at this execution - there is no second bus to open and no
//! namespace to resolve.

use std::sync::Arc;

use anyhow::Result;
use phoxal_api::robot as api;
use phoxal_bus::StateView;
use phoxal_cli_observation::{AttachmentEvent, MotionObservation, ObservationSource, SourceStatus};

use super::FeedContext;

const SOURCE: ObservationSource = ObservationSource::Motion;

pub(crate) async fn run(context: FeedContext) {
    super::until_cancelled(&context, SOURCE, feed(&context)).await;
}

async fn feed(context: &FeedContext) -> Result<()> {
    let state_view = StateView::new(
        context.attachment.bus(),
        &api::topic::client().motion().state(),
    )
    .await?;
    context.health(SOURCE, SourceStatus::Live).await;
    let mut ticker = tokio::time::interval(std::time::Duration::from_millis(20));
    let mut last_sequence = None;
    loop {
        ticker.tick().await;
        let Some(observed) = state_view.observed() else {
            continue;
        };
        let sequence = observed.metadata.sequence;
        if last_sequence == Some(sequence) {
            continue;
        }
        last_sequence = Some(sequence);
        let state = &observed.body;
        let (linear_x_mps, angular_z_radps) = match &state.decision {
            api::motion::Decision::Active { target, .. } => {
                (target.linear_x_mps(), target.angular_z_radps())
            }
            api::motion::Decision::Stopped { .. } => (0.0, 0.0),
        };
        let motion = MotionObservation {
            linear_x_mps,
            angular_z_radps,
        };
        let observation = {
            context.stores.motion.write().await.record(motion);
            context.stores.input.read().await.observe(motion)
        };
        context
            .events
            .send(AttachmentEvent::InputChanged {
                epoch: context.epoch,
                values: Arc::new(observation),
            })
            .await?;
    }
}
