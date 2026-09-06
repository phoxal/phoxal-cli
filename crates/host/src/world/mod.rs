//! Owner-only local discovery and retained evidence for world sessions.
//!
//! The adapter host owns each registration's contents and lease. The CLI owns
//! the per-user roots, exact-ID lookup, process-birth and lease validation,
//! stale cleanup, terminal evidence reads, and bounded terminal-session
//! retention. Mutable world state never appears here; callers obtain it from
//! the framework-owned typed world-session API at the registered endpoint.

use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, bail, ensure};
use phoxal::identity::ExecutionId;
use phoxal::model::world::WorldInstanceId;
use phoxal::supervisor::api::simulation::SimulationEndReason;
pub use phoxal::world::api::session::document::{
    LOCAL_WORLD_REGISTRATION_SCHEMA as REGISTRATION_SCHEMA, LocalWorldRegistration,
    NativeProcessIdentity, ProcessIdentity, RegisteredWorld, TerminalCleanup, TerminalFailure,
    TerminalOutcome, TerminalRetention, WORLD_CHECKPOINT_SCHEMA,
    WORLD_MEMBER_TERMINAL_SCHEMA as MEMBER_TERMINAL_SCHEMA,
    WORLD_TERMINAL_SUMMARY_SCHEMA as TERMINAL_SUMMARY_SCHEMA, WorldCheckpoint, WorldMemberEvidence,
    WorldMemberEvidenceIndex, WorldTerminalSummary,
};
use serde::Serialize;
use sysinfo::{Pid, ProcessesToUpdate, System};

pub const REGISTRY_DIR_ENV: &str = "PHOXAL_SIMULATION_REGISTRY_DIR";
pub const EVIDENCE_DIR_ENV: &str = "PHOXAL_SIMULATION_EVIDENCE_DIR";
pub const LOG_BYTE_LIMIT_ENV: &str = "PHOXAL_SIMULATION_LOG_BYTE_LIMIT";
pub const DEFAULT_TERMINAL_SESSION_LIMIT: usize = 50;
pub const DEFAULT_LOG_BYTE_LIMIT: u64 = 16 * 1024 * 1024;
const STALE_BOOTSTRAP_LOG_AGE: Duration = Duration::from_secs(10 * 60);
const NATIVE_EXIT_GRACE: Duration = Duration::from_secs(3);
const NATIVE_TERM_BUDGET: Duration = Duration::from_secs(20);
const NATIVE_KILL_BUDGET: Duration = Duration::from_secs(1);
const NATIVE_POLL_INTERVAL: Duration = Duration::from_millis(100);

mod evidence;
mod paths;
mod process_control;
mod recovery;
mod registry;

pub use evidence::{PruneReport, WorldEvidence};
pub use paths::{WorldPaths, parse_instance_id, validate_instance_id};
pub use process_control::{
    NativeProcessControl, NativeSignal, ObservedNativeProcess, ProcessInspector,
    SystemProcessInspector,
};
pub use registry::WorldRegistry;

use paths::*;
use process_control::converge_native_process_group;
use registry::{RegistrationProbe, StaleRegistration, ValidateWorldCheckpoint};

#[cfg(test)]
mod tests;
