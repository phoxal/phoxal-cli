use std::path::PathBuf;

use crate::Project;
use crate::Ui;
use anyhow::Result;

#[derive(Debug, Clone)]
pub struct AppContext {
    pub ui: Ui,
    pub project: Project,
    pub catalog_source: Option<String>,
}

impl AppContext {
    pub fn new(workspace_root: PathBuf, catalog_source: Option<String>) -> Result<Self> {
        Ok(Self {
            ui: Ui::new(),
            project: Project::new(workspace_root)?,
            catalog_source,
        })
    }
}
