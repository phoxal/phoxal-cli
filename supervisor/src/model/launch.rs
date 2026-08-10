//! Supervisor-owned process launch values.

use std::path::PathBuf;
use std::time::Duration;

use crate::model::{ParticipantKind, ProcessKey, RuntimeFailurePolicy, StartupRequirement};

pub const RESTART_DELAY: Duration = Duration::from_secs(2);
pub const START_LIMIT_INTERVAL: Duration = Duration::from_secs(60);
pub const START_LIMIT_BURST: usize = 5;

#[derive(Debug, Clone, PartialEq)]
pub struct ParticipantSpec {
    pub spawn: bool,
    pub key: ProcessKey,
    pub kind: ParticipantKind,
    pub executable: PathBuf,
    pub args: Vec<String>,
    pub shutdown_grace: Duration,
    pub startup_requirement: StartupRequirement,
    pub runtime_failure: RuntimeFailurePolicy,
    pub restart_policy: RestartPolicy,
}

impl ParticipantSpec {
    #[must_use]
    pub fn command_line(&self) -> String {
        std::iter::once(self.executable.display().to_string())
            .chain(self.args.iter().cloned())
            .collect::<Vec<_>>()
            .join(" ")
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct RestartPolicy {
    pub restart_delay: Duration,
    pub start_limit_interval: Duration,
    pub start_limit_burst: usize,
}

impl Default for RestartPolicy {
    fn default() -> Self {
        Self {
            restart_delay: RESTART_DELAY,
            start_limit_interval: START_LIMIT_INTERVAL,
            start_limit_burst: START_LIMIT_BURST,
        }
    }
}
