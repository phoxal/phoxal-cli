//! Resolved identity and authority for one resident runtime.

use std::path::PathBuf;

/// Paths and resident authority resolved from one logical runtime root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeTarget {
    pub logical_root: PathBuf,
    pub requested_entry: Option<PathBuf>,
    pub build_lock: PathBuf,
    pub supervisor_socket: PathBuf,
    pub zenoh_socket: PathBuf,
    pub zenoh_endpoint: String,
    pub authority: ResidentAuthority,
}

/// Process owner responsible for one resident runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResidentAuthority {
    DetachedSession,
    SystemdUnit { unit: String },
}
