use std::io::IsTerminal;
use std::path::PathBuf;

use crate::cli::Ui;
use crate::cli::output::OutputContext;
use anyhow::Result;
use phoxal_cli_core::Project;

/// Disables network access for `cargo install`/`cargo metadata`
/// materialization: every invocation this CLI makes passes `--offline` when
/// set.
pub(crate) const OFFLINE_ENV: &str = "PHOXAL_OFFLINE";

#[derive(Debug, Clone)]
pub struct AppContext {
    pub ui: Ui,
    pub project: Project,
    pub offline: bool,
    /// The output contract for the `run`/`simulation webots run` attachment application.
    /// [`AppContext::new`] computes it once from stderr's terminal state.
    pub output: OutputContext,
}

impl AppContext {
    pub fn new(workspace_root: PathBuf, offline: bool) -> Result<Self> {
        // SAFETY: no task that reads the project-root environment has been
        // spawned on this path. Path helpers use this value only after the
        // context is fully constructed.
        unsafe {
            std::env::set_var(phoxal_cli_project::PROJECT_ROOT_ENV, &workspace_root);
        }
        let output = OutputContext::compute(std::io::stderr().is_terminal());
        Ok(Self {
            ui: Ui::new(output.decorated()),
            project: Project::new(workspace_root)?,
            offline,
            output,
        })
    }
}

/// Whether `PHOXAL_OFFLINE` is set, for callers that only have the env (not
/// an [`AppContext`]) to hand - the identical check `--offline` performs.
#[must_use]
pub(crate) fn offline_from_env() -> bool {
    offline_from_value(std::env::var(OFFLINE_ENV).ok().as_deref())
}

#[must_use]
pub(crate) fn offline_from_value(value: Option<&str>) -> bool {
    value.is_some_and(|value| {
        !value.is_empty() && !matches!(value.to_ascii_lowercase().as_str(), "0" | "false" | "no")
    })
}

#[cfg(test)]
mod tests {
    use super::offline_from_value;

    #[test]
    fn offline_environment_accepts_shell_friendly_boolean_values() {
        for value in ["1", "true", "TRUE", "yes", "on"] {
            assert!(offline_from_value(Some(value)), "{value}");
        }
        for value in ["", "0", "false", "FALSE", "no"] {
            assert!(!offline_from_value(Some(value)), "{value}");
        }
        assert!(!offline_from_value(None));
    }
}
