//! The one shared "what role does this participant play" enum.
//!
//! Before this module, three call sites each grew their own overlapping
//! kind enum: `supervisor::ParticipantKind` (`SiteTool`, `OfficialArtifact`,
//! `UserService`, `ComponentDriver`), `commands::check::SourceParticipantKind`
//! (`UserService`, `OfficialService`, `ComponentDriver`, `Tool`, `Simulator`),
//! and `watch::WatchTargetKind` (`Service`, `Driver`, `Tool`, `Simulator`).
//! All three were really the same four-way role split wearing different
//! names, plus - in the supervisor and check cases - an orthogonal bit for
//! "did this come from a local directory (user-authored source, or a local
//! path-pin override) or a fetched catalog artifact". [`ParticipantKind`] is
//! that shared role split; each call site keeps its own bit (or bits) for
//! the orthogonal distinction it still needs (see `supervisor::ParticipantSpec::local`
//! /`supervisor::ParticipantStatus::local` and
//! `check::SourceParticipantKind::official`) rather than folding everything
//! into one lossy type.
use serde::{Deserialize, Serialize};

/// What role a participant plays in a robot's contract graph: a CLI-managed
/// peripheral tool (the router transport, the joypad, the Webots
/// application), a bus service, a component driver, or a simulator (the
/// Webots supervisor or a robot's controller). Orthogonal to whether this
/// particular incarnation is running from a locally resolved directory or a
/// fetched catalog artifact - callers that need that distinction carry it
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
