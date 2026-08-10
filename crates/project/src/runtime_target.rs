//! Resolved project identity and authority for one supervised runtime.

use std::path::PathBuf;

/// Paths and process authority resolved from one logical runtime root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeTarget {
    pub logical_root: PathBuf,
    pub requested_entry: Option<PathBuf>,
    pub build_lock: PathBuf,
    pub supervisor_socket: PathBuf,
    pub zenoh_endpoint: String,
    pub authority: RuntimeAuthority,
}

/// Process owner responsible for one supervised runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeAuthority {
    DetachedSession,
    SystemdUnit { unit: String },
}
