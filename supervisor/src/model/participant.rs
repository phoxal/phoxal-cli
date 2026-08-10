//! The supervisor role a participant plays in an execution graph.
//!
//! Source origin is orthogonal: command and supervisor adapters carry their
//! own local/registry bit alongside this enum when that distinction matters.
use serde::{Deserialize, Serialize};

/// What role a process plays in a robot's contract graph: the one mandatory
/// root brain, a bus service, a component driver, or a simulator controller.
/// Orthogonal to whether this particular process is
/// running from a locally resolved directory or was materialized from the
/// registry (`cargo install`, at the locked train) - callers that need that
/// distinction carry it alongside, not inside, this enum (see the module
/// docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParticipantKind {
    /// The one mandatory root brain: the robot project's
    /// composition root, built from the root Cargo package and staged as
    /// `bin/brain`. A checked, clocked robot-graph participant, distinct from
    /// a user service and never collapsed into one.
    Brain,
    Service,
    Driver,
    Simulator,
}
