//! One supervised process, as a client renders it.

use std::collections::BTreeMap;
use std::time::Instant;

use phoxal_api::supervisor::snapshot::Process;
use phoxal_runtime_contract::identity::ParticipantId;
use phoxal_runtime_contract::metadata::ParticipantKind;

/// A snapshot row plus the local timing a client keeps for it.
///
/// The row itself is the daemon's authoritative value and is carried whole
/// rather than destructured: adding a field to the contract must not mean
/// editing a projection here. The timings are client-local wall-clock facts
/// (when this client first saw the row start, become ready, or end) and belong
/// to no one else.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessObservation {
    pub row: Process,
    pub observed_started_at: Instant,
    pub observed_ended_at: Option<Instant>,
    pub observed_first_ready_at: Option<Instant>,
}

impl ProcessObservation {
    /// The participant kind this row denotes, which its key already carries.
    #[must_use]
    pub const fn kind(&self) -> ParticipantKind {
        self.row.kind
    }

    /// Whether the process has an open bus session, which is exactly whether
    /// the daemon learned a producer from its liveliness token.
    #[must_use]
    pub const fn present(&self) -> bool {
        self.row.producer.is_some()
    }
}

pub type ProcessTable = BTreeMap<ParticipantId, ProcessObservation>;
