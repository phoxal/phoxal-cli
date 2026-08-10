//! The daemon's internal execution board.
//!
//! This is deliberately **not** a wire document. The store is the authority the
//! process machinery writes to on every spawn, exit, and liveliness token, so
//! typing it on a published DTO would make every such write a protocol edit -
//! and would drag a supervisor generation, a router endpoint, a framework
//! train, and a manual-input capability into a value none of them belong to.
//! [`crate::daemon::projection`] is the one place this
//! becomes a snapshot a client is told about.

use std::collections::BTreeMap;

use crate::model::{ProcessEntry, ProcessKey, ProjectLifecycle};

/// Everything the process machinery authoritatively knows.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Board {
    pub(crate) lifecycle: ProjectLifecycle,
    pub(crate) processes: BTreeMap<ProcessKey, ProcessEntry>,
    /// The first supervision-level failure reason, when no single process is
    /// to blame. The daemon classifies it into a typed `DaemonFailure`; this
    /// is the evidence it classifies.
    pub(crate) failure: Option<String>,
}

impl Default for Board {
    fn default() -> Self {
        Self {
            lifecycle: ProjectLifecycle::Starting,
            processes: BTreeMap::new(),
            failure: None,
        }
    }
}
