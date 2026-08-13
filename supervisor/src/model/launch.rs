//! Supervisor-owned process launch values.

use std::path::PathBuf;
use std::time::Duration;

use super::participant::ParticipantKind;
use super::process::ProcessKey;

const RESTART_DELAY: Duration = Duration::from_secs(2);
const START_LIMIT_INTERVAL: Duration = Duration::from_secs(60);
const START_LIMIT_BURST: usize = 5;

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ParticipantSpec {
    pub(crate) spawn: bool,
    pub(crate) key: ProcessKey,
    pub(crate) kind: ParticipantKind,
    pub(crate) executable: PathBuf,
    pub(crate) args: Vec<String>,
    pub(crate) shutdown_grace: Duration,
    pub(crate) restart_policy: RestartPolicy,
}

impl ParticipantSpec {
    #[must_use]
    pub(crate) fn command_line(&self) -> String {
        std::iter::once(self.executable.display().to_string())
            .chain(self.args.iter().cloned())
            .collect::<Vec<_>>()
            .join(" ")
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RestartPolicy {
    pub(crate) restart_delay: Duration,
    pub(crate) start_limit_interval: Duration,
    pub(crate) start_limit_burst: usize,
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
