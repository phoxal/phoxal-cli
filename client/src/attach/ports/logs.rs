use std::sync::Arc;

use phoxal_cli_observation::{LogRead, LogWindow};
use tokio::sync::RwLock;

use crate::attach::state::logs::LogStore;

#[derive(Clone)]
pub(crate) struct LogReader {
    store: Arc<RwLock<LogStore>>,
}

impl LogReader {
    pub(crate) const fn new(store: Arc<RwLock<LogStore>>) -> Self {
        Self { store }
    }

    pub(crate) async fn read(&self, query: LogRead) -> LogWindow {
        self.store.write().await.read(query)
    }
}
