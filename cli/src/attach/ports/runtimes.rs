use std::sync::Arc;

use phoxal_cli_observation::{RuntimeRead, RuntimeWindow};
use tokio::sync::RwLock;

use crate::attach::state::runtimes::RuntimeStore;

#[derive(Clone)]
pub(crate) struct RuntimeReader {
    store: Arc<RwLock<RuntimeStore>>,
}

impl RuntimeReader {
    pub(crate) const fn new(store: Arc<RwLock<RuntimeStore>>) -> Self {
        Self { store }
    }

    pub(crate) async fn read(&self, query: RuntimeRead) -> RuntimeWindow {
        self.store.write().await.read(query)
    }
}
