//! Project contract-checking primitives that do not own command presentation.

pub mod participant_metadata;
pub mod source;
mod vocabulary;

pub use vocabulary::{ParticipantApis, ParticipantKind, ParticipantScope, Problem, Report};
