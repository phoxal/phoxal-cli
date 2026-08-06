use std::sync::Arc;

use phoxal_cli_observation::{LogRead, LogWindow};
use tokio::sync::RwLock;

use crate::state::logs::LogStore;

#[derive(Clone)]
pub struct LogReader {
    pub(crate) store: Arc<RwLock<LogStore>>,
}

impl LogReader {
    pub async fn read(&self, query: LogRead) -> LogWindow {
        self.store.write().await.read(query)
    }
}
