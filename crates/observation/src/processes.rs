use std::collections::BTreeMap;
use std::time::Instant;

use phoxal_cli_core::runtime::{
    ParticipantKind, ParticipantState, ProcessEntry, ProcessKey, RobotKey,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessObservation {
    pub key: ProcessKey,
    pub entry: ProcessEntry,
    pub kind: ParticipantKind,
    pub state: ParticipantState,
    pub present: Option<bool>,
    pub robot: Option<RobotKey>,
    pub started_at: Instant,
    pub ended_at: Option<Instant>,
    pub first_ready_at: Option<Instant>,
    pub user_service: bool,
}

pub type ProcessTable = BTreeMap<ProcessKey, ProcessObservation>;
