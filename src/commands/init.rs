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
        initialize(app.project.root(), app.offline)
    }
}

/// Create the train-anchor `Cargo.toml`/`src/lib.rs` and generate the
/// project's first `Cargo.lock`.
///
/// `offline` (organization#951 WS4 review, round 2): `--offline init` is
/// SUPPORTED, not rejected, for consistency with every other Cargo
/// invocation this CLI threads `--offline` through - the same principle
/// applies here as everywhere else: if the local registry cache already has
/// the exact pinned `phoxal` version (a prior project on this machine, or a
/// pre-warmed air-gapped mirror), `cargo generate-lockfile --offline`
/// succeeds with no network at all; if it does not, Cargo's own precise
/// offline error is the honest outcome, not a silent network call the user
/// explicitly asked this invocation not to make. Rejecting the combination
/// outright would special-case `init` against that same principle for a
/// case that is not actually impossible, only unlikely to succeed on a
/// completely cold cache.
pub fn initialize(project_root: &Path, offline: bool) -> Result<()> {
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
        .args(generate_lockfile_args(offline))
        .current_dir(project_root)
        .output()
        .context("Cargo is required to create the project train lock")?;
    if !output.status.success() {
        let _ = fs::remove_file(&manifest);
        let _ = fs::remove_file(&source);
        let _ = fs::remove_file(&lock);
        let _ = fs::remove_dir(&source_dir);
        let offline_hint = if offline {
            " (--offline was requested; the exact pinned phoxal version must already be in the \
             local registry cache, e.g. from a prior project on this machine - retry without \
             --offline to fetch it once)"
        } else {
            ""
        };
        bail!(
            "Cargo.lock generation failed and the incomplete train anchor was removed{offline_hint}: {}",
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

/// The `cargo generate-lockfile` argv, with `--offline` appended exactly
/// when requested - pure and unit-testable without spawning Cargo.
fn generate_lockfile_args(offline: bool) -> Vec<&'static str> {
    let mut args = vec!["generate-lockfile"];
    if offline {
        args.push("--offline");
    }
    args
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refuses_to_overwrite_any_anchor_file() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("Cargo.toml"), "existing").unwrap();
        let error = initialize(root.path(), false).unwrap_err();
        assert!(error.to_string().contains("refusing to overwrite"));
        assert_eq!(
            fs::read_to_string(root.path().join("Cargo.toml")).unwrap(),
            "existing"
        );
    }

    /// Organization#951 WS4 review, round 2: `--offline init` is supported
    /// (see [`initialize`]'s doc comment for the reasoning), which means the
    /// actual `cargo generate-lockfile` invocation must carry the flag.
    #[test]
    fn generate_lockfile_args_appends_offline_only_when_requested() {
        assert_eq!(generate_lockfile_args(false), vec!["generate-lockfile"]);
        assert_eq!(
            generate_lockfile_args(true),
            vec!["generate-lockfile", "--offline"]
        );
    }
}
