use std::sync::Arc;

use phoxal_cli_observation::{BusRead, BusWindow};
use tokio::sync::RwLock;

use crate::state::bus::BusStore;

#[derive(Clone)]
pub struct BusReader {
    pub(crate) store: Arc<RwLock<BusStore>>,
}

impl BusReader {
    pub async fn read(&self, query: BusRead) -> BusWindow {
        self.store.write().await.read(query)
    }
}
