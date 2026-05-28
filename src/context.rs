use std::path::PathBuf;

use crate::Project;
use crate::Ui;
use anyhow::Result;

#[derive(Debug, Clone)]
pub struct AppContext {
    pub ui: Ui,
    pub project: Project,
}

impl AppContext {
    pub fn new(workspace_root: PathBuf) -> Result<Self> {
        Ok(Self {
            ui: Ui::new(),
            project: Project::new(workspace_root)?,
        })
    }
}
