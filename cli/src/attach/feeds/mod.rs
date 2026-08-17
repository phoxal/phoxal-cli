//! The feeds that turn one attachment into the observations a terminal renders.
//!
//! Every feed is disposable and epoch-stamped. None of them owns retained
//! state: the stores behind the ports do, and a feed only ever hands them a
//! value and announces the revision.

pub(crate) mod input;
pub(crate) mod logs;
pub(crate) mod motion;
pub(crate) mod snapshot;
pub(crate) mod telemetry;

use std::sync::Arc;

use phoxal_cli_observation::{AttachmentEpoch, AttachmentEvent, ObservationSource, SourceStatus};
use phoxal_client::Client;
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use super::ports::input::InputCommand;
use super::state::Stores;

/// Everything a feed needs, cloned into each of them.
#[derive(Clone)]
pub(crate) struct FeedContext {
    pub(crate) client: Client,
    pub(crate) epoch: AttachmentEpoch,
    /// Where this client believes the execution's bundle lives, for display.
    pub(crate) project: String,
    /// What this client knows about the runtimes it launched itself. Empty for
    /// an attachment to an execution somebody else started.
    pub(crate) local: super::LocalRuntimeFacts,
    pub(crate) stores: Stores,
    pub(crate) events: mpsc::Sender<AttachmentEvent>,
    pub(crate) cancellation: CancellationToken,
}

impl FeedContext {
    /// Announce one source's health. A feed that cannot reach its endpoint is
    /// not a failed attachment: the snapshot keeps flowing and the operator is
    /// told which view is stale.
    pub(crate) async fn health(&self, source: ObservationSource, status: SourceStatus) {
        let mut health = self.stores.health.write().await;
        let Some(values) = health.record(source, status) else {
            return;
        };
        let _ = self
            .events
            .send(AttachmentEvent::SourceHealthChanged {
                epoch: self.epoch,
                values: Arc::new(values),
            })
            .await;
    }
}

/// Start every feed for one session.
pub(crate) fn spawn_all(
    tasks: &mut JoinSet<()>,
    context: FeedContext,
    input_rx: mpsc::Receiver<InputCommand>,
) {
    tasks.spawn(snapshot::run(context.clone()));
    tasks.spawn(logs::run(context.clone()));
    tasks.spawn(telemetry::run(context.clone()));
    tasks.spawn(motion::run(context.clone()));
    tasks.spawn(input::run(context, input_rx));
}

/// Run `feed` until the session is cancelled, reporting a failure as source
/// health rather than as the end of the attachment.
pub(crate) async fn until_cancelled<F>(context: &FeedContext, source: ObservationSource, feed: F)
where
    F: Future<Output = anyhow::Result<()>>,
{
    tokio::select! {
        () = context.cancellation.cancelled() => {}
        result = feed => {
            if let Err(error) = result {
                tracing::debug!(source = source.label(), error = %format!("{error:#}"), "an attachment feed stopped");
                context.health(source, SourceStatus::Failed).await;
            }
        }
    }
}
