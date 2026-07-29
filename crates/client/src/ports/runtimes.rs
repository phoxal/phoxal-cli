use std::sync::Arc;

use phoxal_cli_observation::{RuntimeRead, RuntimeWindow};
use tokio::sync::RwLock;

use crate::state::runtimes::RuntimeStore;

#[derive(Clone)]
pub struct RuntimeReader {
    pub(crate) store: Arc<RwLock<RuntimeStore>>,
}

impl RuntimeReader {
    pub async fn read(&self, query: RuntimeRead) -> RuntimeWindow {
        self.store.write().await.read(query)
    }
}
