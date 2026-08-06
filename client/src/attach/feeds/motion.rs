//! The robot's arbitrated motion state.
//!
//! This is the one feed that reads a robot-domain topic rather than the
//! supervisor contract. It opens on the attachment's own session, so its key
//! is already rooted at this execution - there is no second bus to open and no
//! namespace to resolve (organization#978).

use std::sync::Arc;

use anyhow::Result;
use phoxal_api::v0_1 as api;
use phoxal_bus::{ContractBody, Subscribe, Subscriber, Topic};
use phoxal_cli_observation::{AttachmentEvent, MotionObservation, SourceStatus};

use super::FeedContext;

const SOURCE: &str = "motion";

pub(crate) async fn run(context: FeedContext) {
    super::until_cancelled(&context, SOURCE, feed(&context)).await;
}

async fn feed(context: &FeedContext) -> Result<()> {
    let topic = Topic::<Subscribe<api::motion::State>>::new_static(
        <api::motion::State as ContractBody>::TOPIC,
    );
    let subscriber = Subscriber::new(context.attachment.bus(), &topic, 32).await?;
    context.health(SOURCE, SourceStatus::Live).await;
    loop {
        let state = subscriber.recv().await?.body;
        let motion = MotionObservation {
            linear_x_mps: state.linear_x_mps,
            angular_z_radps: state.angular_z_radps,
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
