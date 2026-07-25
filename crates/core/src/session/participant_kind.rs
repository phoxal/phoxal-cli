//! The shared role a participant plays in a robot contract graph.
//!
//! Source origin is orthogonal: command and supervisor adapters carry their
//! own local/suite bit alongside this enum when that distinction matters.
use serde::{Deserialize, Serialize};

/// What role a participant plays in a robot's contract graph: a CLI-managed
/// peripheral tool (the router transport, the joypad, the Webots
/// application), a bus service, a component driver, or a simulator (the
/// Webots application or a robot's controller). Orthogonal to whether this
/// particular incarnation is running from a locally resolved directory or a
/// fetched suite artifact - callers that need that distinction carry it
/// alongside, not inside, this enum (see the module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParticipantKind {
    Tool,
    Service,
    Driver,
    Simulator,
}

impl ParticipantKind {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Tool => "tool",
            Self::Service => "service",
            Self::Driver => "driver",
            Self::Simulator => "simulator",
        }
    }
}
