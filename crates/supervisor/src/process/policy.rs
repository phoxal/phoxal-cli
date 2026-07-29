//! Restart and start-limit policies owned by the resident supervisor.

use std::time::Duration;

pub(crate) use phoxal_cli_core::runtime::RestartPolicy;

#[derive(Debug, Clone)]
pub(crate) struct RouterRecoveryPolicy {
    pub(crate) restart_delay: Duration,
    pub(crate) start_limit_interval: Duration,
    pub(crate) start_limit_burst: usize,
}

impl Default for RouterRecoveryPolicy {
    fn default() -> Self {
        Self {
            restart_delay: phoxal_cli_core::runtime::launch::RESTART_DELAY,
            start_limit_interval: phoxal_cli_core::runtime::launch::START_LIMIT_INTERVAL,
            start_limit_burst: phoxal_cli_core::runtime::launch::START_LIMIT_BURST,
        }
    }
}
