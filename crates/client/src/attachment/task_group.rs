use tokio::task::JoinSet;

pub(crate) struct TaskGroup {
    tasks: JoinSet<()>,
}

impl TaskGroup {
    pub fn new() -> Self {
        Self {
            tasks: JoinSet::new(),
        }
    }

    pub fn spawn(&mut self, future: impl Future<Output = ()> + Send + 'static) {
        self.tasks.spawn(future);
    }

    pub async fn join(mut self) {
        while let Some(result) = self.tasks.join_next().await {
            if let Err(error) = result {
                tracing::debug!(error = %error, "attachment source task failed");
            }
        }
    }
}
