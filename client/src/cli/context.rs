use std::io::IsTerminal;
use std::path::PathBuf;

use crate::cli::output::OutputContext;
use crate::cli::output::plain::Ui;
use anyhow::Result;
use phoxal_cli_project::source::Project;

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
        let output = OutputContext::compute(std::io::stderr().is_terminal());
        Ok(Self {
            ui: Ui::new(output.decorated(), false),
            project: Project::new(workspace_root)?,
            offline,
            output,
        })
    }
}
