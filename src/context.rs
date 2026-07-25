use std::io::IsTerminal;
use std::path::PathBuf;

use crate::Ui;
use crate::session::output::OutputContext;
use anyhow::Result;
use phoxal_cli_core::Project;

#[derive(Debug, Clone)]
pub struct AppContext {
    pub ui: Ui,
    pub project: Project,
    pub suite_source: Option<String>,
    pub offline: bool,
    /// The output contract for `run`/`simulation run`'s `SessionController`.
    /// [`AppContext::new`] computes it once from stderr's terminal state.
    pub output: OutputContext,
}

impl AppContext {
    pub fn new(
        workspace_root: PathBuf,
        suite_source: Option<String>,
        offline: bool,
    ) -> Result<Self> {
        // SAFETY: AppContext is constructed once during single-threaded CLI
        // startup, before workers are spawned. Path helpers use this to keep
        // every mutable artifact under the selected runtime's path policy.
        unsafe {
            std::env::set_var(crate::host_paths::PROJECT_ROOT_ENV, &workspace_root);
            if offline {
                std::env::set_var(phoxal_cli_core::project::suite::OFFLINE_ENV, "1");
            }
        }
        let output = OutputContext::compute(std::io::stderr().is_terminal());
        Ok(Self {
            ui: Ui::new(output.decorated()),
            project: Project::new(workspace_root)?,
            suite_source,
            offline,
            output,
        })
    }
}
