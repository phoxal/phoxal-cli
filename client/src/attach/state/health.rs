//! Per-source health, so a stale view is named rather than silently empty.

use phoxal_cli_observation::{SourceHealth, SourceStatus};

#[derive(Default)]
pub(crate) struct HealthStore {
    health: SourceHealth,
}

impl HealthStore {
    /// Record one source's status, returning the new value only when it
    /// actually changed - a feed reporting `Live` every page must not wake the
    /// renderer.
    pub fn record(&mut self, source: &str, status: SourceStatus) -> Option<SourceHealth> {
        if self.health.sources.get(source) == Some(&status) {
            return None;
        }
        self.health.sources.insert(source.to_string(), status);
        Some(self.health.clone())
    }
}
