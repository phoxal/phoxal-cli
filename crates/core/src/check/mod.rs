//! Contract-checking primitives that do not own command presentation.

pub mod participant_metadata;
pub mod source;
mod vocabulary;

pub use vocabulary::{
    ParticipantApis, ParticipantClass, ParticipantKind, ParticipantScope, Problem, Report,
};
