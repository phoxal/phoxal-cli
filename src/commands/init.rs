use std::fs;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};
use clap::Args;

use crate::AppContext;

#[derive(Debug, Args)]
pub struct Init {}

impl Init {
    pub async fn run(&self, app: &AppContext) -> Result<()> {
        initialize(app.project.root())
    }
}

pub fn initialize(project_root: &Path) -> Result<()> {
    let manifest = project_root.join("Cargo.toml");
    let source = project_root.join("src/lib.rs");
    let lock = project_root.join("Cargo.lock");
    if manifest.exists() || source.exists() || lock.exists() {
        bail!(
            "refusing to overwrite an existing train anchor at {}; review Cargo.toml, src/lib.rs, and Cargo.lock manually",
            project_root.display()
        );
    }

    let source_dir = project_root.join("src");
    fs::create_dir_all(&source_dir)?;
    fs::write(
        &manifest,
        format!(
            "[package]\nname = \"phoxal-robot-project\"\nversion = \"0.0.0\"\nedition = \"2024\"\npublish = false\n\n[workspace]\nresolver = \"3\"\nmembers = []\n\n[workspace.dependencies]\nphoxal = \"={}\"\n\n[dependencies]\nphoxal.workspace = true\n",
            phoxal::VERSION
        ),
    )?;
    fs::write(
        &source,
        "//! Robot-project framework-train anchor.\n\npub const FRAMEWORK_TRAIN: &str = phoxal::VERSION;\n",
    )?;
    let output = Command::new("cargo")
        .arg("generate-lockfile")
        .current_dir(project_root)
        .output()
        .context("Cargo is required to create the project train lock")?;
    if !output.status.success() {
        let _ = fs::remove_file(&manifest);
        let _ = fs::remove_file(&source);
        let _ = fs::remove_file(&lock);
        let _ = fs::remove_dir(&source_dir);
        bail!(
            "Cargo.lock generation failed and the incomplete train anchor was removed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    println!(
        "initialized framework train {} in {}",
        phoxal::VERSION,
        project_root.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refuses_to_overwrite_any_anchor_file() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("Cargo.toml"), "existing").unwrap();
        let error = initialize(root.path()).unwrap_err();
        assert!(error.to_string().contains("refusing to overwrite"));
        assert_eq!(
            fs::read_to_string(root.path().join("Cargo.toml")).unwrap(),
            "existing"
        );
    }
}
