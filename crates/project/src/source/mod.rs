//! Authored-project path conventions and source resolution.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

pub mod intent;
pub mod requirements;
pub mod resolver;
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

/// The host's target triple, in the shape official Cargo packages are
/// compiled for.
///
/// It is a pure function of the compiled host, with no environment override:
/// a caller that wants a *different* target passes one - that is what
/// `phoxal build --target` is - and this is only ever the fallback when none
/// was named.
#[must_use]
pub fn host_target_triple() -> String {
    let arch = std::env::consts::ARCH;
    let os = match std::env::consts::OS {
        "macos" => "apple-darwin",
        "linux" => "unknown-linux-gnu",
        other => other,
    };
    format!("{arch}-{os}")
}
