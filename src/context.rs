use std::io::IsTerminal;
use std::path::PathBuf;

use crate::Project;
use crate::Ui;
use crate::commands::MessageFormat;
use crate::session::output::OutputContext;
use anyhow::Result;

#[derive(Debug, Clone)]
pub struct AppContext {
    pub ui: Ui,
    pub project: Project,
    pub catalog_source: Option<String>,
    pub offline: bool,
    pub quiet: bool,
    /// The output contract for `run`/`simulation run`'s `SessionController`.
    /// [`AppContext::new`] fills this with a reasonable default computed from
    /// the live environment (matching [`crate::ui::Ui::from_env`]'s own
    /// fallback), since `--plain`/`--message-format` are not yet known at
    /// construction time; [`crate::commands::dispatch`] overwrites it (and
    /// `ui`'s mode alongside it) with the precise value computed from the
    /// actual CLI invocation before any command runs (see that function's
    /// docs).
    pub output: OutputContext,
}

impl AppContext {
    pub fn new(
        workspace_root: PathBuf,
        catalog_source: Option<String>,
        offline: bool,
        quiet: bool,
    ) -> Result<Self> {
        // SAFETY: AppContext is constructed once during single-threaded CLI
        // startup, before workers are spawned. Path helpers use this to keep
        // every mutable artifact under the selected project's `.phoxal/`.
        unsafe {
            std::env::set_var(crate::host_paths::PROJECT_ROOT_ENV, &workspace_root);
            if offline {
                std::env::set_var(crate::catalog::OFFLINE_ENV, "1");
            }
            if quiet {
                std::env::set_var("PHOXAL_QUIET", "1");
            }
        }
        let output = OutputContext::compute(
            std::io::stderr().is_terminal(),
            false,
            quiet,
            MessageFormat::Human,
        );
        Ok(Self {
            ui: Ui::new(output.mode),
            project: Project::new(workspace_root)?,
            catalog_source,
            offline,
            quiet,
            output,
        })
    }
}
