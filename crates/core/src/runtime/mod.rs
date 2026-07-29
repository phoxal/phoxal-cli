//! Pure runtime domain values shared by CLI owners.

pub mod launch;
pub mod paths;
mod target;

pub use launch::{ParticipantSpec, RestartPolicy};
pub use target::{ResidentAuthority, RuntimeTarget};

pub const PROJECT_ROOT_ENV: &str = "PHOXAL_PROJECT_ROOT";

/// Domain bounds shared by launch-plan construction and its wire projection.
///
/// These belong to the runtime model because plans must be rejected before a
/// supervisor publishes partial state. The protocol crate aliases them while
/// retaining ownership of encoded frame-size limits.
pub const MAX_SUPERVISED_PROCESSES: usize = 40;
pub const MAX_RUNTIME_ARTIFACT_ID_BYTES: usize = 1024;
pub const MAX_RUNTIME_TEXT_BYTES: usize = 4 * 1024;
