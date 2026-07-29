use phoxal_cli_core::runtime::ProcessKey;
use phoxal_cli_observation::{AttachmentEpoch, QueryToken, RuntimeRow, StoreRevision};

#[derive(Debug, Clone, PartialEq)]
pub struct RuntimesModel {
    pub rows: Vec<RuntimeRow>,
    pub candidate: Option<ProcessKey>,
    pub detail: Option<ProcessKey>,
    pub scroll: usize,
    pub known_revision: StoreRevision,
    pub dirty_revision: Option<StoreRevision>,
    pub in_flight: Option<(AttachmentEpoch, StoreRevision, QueryToken)>,
    pub next_token: u64,
}

impl Default for RuntimesModel {
    fn default() -> Self {
        Self {
            rows: Vec::new(),
            candidate: None,
            detail: None,
            scroll: 0,
            known_revision: StoreRevision(0),
            dirty_revision: None,
            in_flight: None,
            next_token: 0,
        }
    }
}
