//! The shared role a participant plays in a robot contract graph.
//!
//! Source origin is orthogonal: command and supervisor adapters carry their
//! own local/registry bit alongside this enum when that distinction matters.
use serde::{Deserialize, Serialize};

/// What role a process plays in a robot's contract graph: a bus service, a
/// component driver, a simulator (a robot's Webots controller), or a
/// CLI-managed host application that is not a bus participant at all (the
/// Webots binary itself). Orthogonal to whether this particular process is
/// running from a locally resolved directory or was materialized from the
/// registry (`cargo install`, at the locked train) - callers that need that
/// distinction carry it alongside, not inside, this enum (see the module
/// docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParticipantKind {
    Host,
    Service,
    Driver,
    Simulator,
}

impl ParticipantKind {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Host => "host",
            Self::Service => "service",
            Self::Driver => "driver",
            Self::Simulator => "simulator",
        }
    }
}
