use std::path::PathBuf;

use crate::Project;
use crate::Ui;
use anyhow::Result;

#[derive(Debug, Clone)]
pub struct AppContext {
    pub ui: Ui,
    pub project: Project,
    pub catalog_source: Option<String>,
    pub offline: bool,
    pub quiet: bool,
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
        Ok(Self {
            ui: Ui::new(),
            project: Project::new(workspace_root)?,
            catalog_source,
            offline,
            quiet,
        })
    }
}
