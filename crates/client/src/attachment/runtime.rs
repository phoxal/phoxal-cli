use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

pub struct AttachmentRuntime {
    cancellation: CancellationToken,
    tasks: JoinSet<()>,
}

impl AttachmentRuntime {
    pub(crate) fn new(cancellation: CancellationToken, tasks: JoinSet<()>) -> Self {
        Self {
            cancellation,
            tasks,
        }
    }

    pub async fn shutdown(mut self) {
        self.cancellation.cancel();
        while self.tasks.join_next().await.is_some() {}
    }
}
