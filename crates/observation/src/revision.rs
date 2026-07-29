use std::sync::Arc;

use crate::AttachmentEpoch;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct StoreRevision(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct QueryToken(pub u64);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreChanged {
    pub epoch: AttachmentEpoch,
    pub revision: StoreRevision,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservationQuery<Q> {
    pub epoch: AttachmentEpoch,
    pub observed_revision: StoreRevision,
    pub token: QueryToken,
    pub body: Q,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservationWindow<T> {
    pub epoch: AttachmentEpoch,
    pub revision: StoreRevision,
    pub token: QueryToken,
    pub rows: Arc<[T]>,
}
