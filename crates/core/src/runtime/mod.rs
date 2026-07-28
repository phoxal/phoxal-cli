//! Pure runtime domain values shared by CLI owners.

mod target;

pub use target::{ResidentAuthority, RuntimeTarget};

/// Domain bounds shared by launch-plan construction and its wire projection.
///
/// These belong to the runtime model because plans must be rejected before a
/// supervisor publishes partial state. The protocol crate aliases them while
/// retaining ownership of encoded frame-size limits.
pub const MAX_SUPERVISED_PROCESSES: usize = 40;
pub const MAX_RUNTIME_ARTIFACT_ID_BYTES: usize = 1024;
pub const MAX_RUNTIME_TEXT_BYTES: usize = 4 * 1024;
