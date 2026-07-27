//! Project-root path conventions and tooling used by CLI domain operations.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

pub mod catalog;
pub mod launch_plan;
pub mod layout;
pub mod resolver;
pub mod suite;
pub mod tooling;
pub mod train;

#[derive(Debug, Clone)]
pub struct Project {
    workspace_root: PathBuf,
}

impl Project {
    pub fn new(workspace_root: impl AsRef<Path>) -> Result<Self> {
        let workspace_root = normalize_existing_path(workspace_root.as_ref())?;
        Ok(Self { workspace_root })
    }

    pub fn root(&self) -> &Path {
        &self.workspace_root
    }
}

fn normalize_existing_path(path: &Path) -> Result<PathBuf> {
    std::env::current_dir()
        .context("failed to resolve current working directory")?
        .join(path)
        .canonicalize()
        .with_context(|| format!("failed to canonicalize {}", path.display()))
}
