use phoxal_cli_observation::{AttachmentEpoch, BusRow, QueryToken, StoreRevision};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BusSort {
    #[default]
    Rate,
    Topic,
    Producer,
}

impl BusSort {
    pub const fn cycle(self) -> Self {
        match self {
            Self::Rate => Self::Topic,
            Self::Topic => Self::Producer,
            Self::Producer => Self::Rate,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct BusModel {
    pub rows: Vec<BusRow>,
    pub filter: String,
    pub sort: BusSort,
    pub show_internal: bool,
    pub editing: bool,
    pub control_candidate: usize,
    pub scroll: usize,
    pub known_revision: StoreRevision,
    pub dirty_revision: Option<StoreRevision>,
    pub in_flight: Option<(AttachmentEpoch, StoreRevision, QueryToken)>,
    pub next_token: u64,
}

impl Default for BusModel {
    fn default() -> Self {
        Self {
            rows: Vec::new(),
            filter: String::new(),
            sort: BusSort::Rate,
            show_internal: false,
            editing: false,
            control_candidate: 0,
            scroll: 0,
            known_revision: StoreRevision(0),
            dirty_revision: None,
            in_flight: None,
            next_token: 0,
        }
    }
}
