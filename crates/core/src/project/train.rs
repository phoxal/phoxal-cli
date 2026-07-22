//! Exact framework-train resolution from a robot project's locked Cargo graph.

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LockedTrain {
    pub version: String,
    pub source: TrainSource,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TrainSource {
    Registry,
    Git(String),
    Path,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RegistryStatus {
    Available,
    Yanked,
}

impl LockedTrain {
    #[must_use]
    pub fn is_published(&self) -> bool {
        matches!(self.source, TrainSource::Registry)
    }
}

pub fn resolve_locked_train(project_root: &Path) -> Result<LockedTrain> {
    let manifest = project_root.join("Cargo.toml");
    let lock = project_root.join("Cargo.lock");
    ensure!(
        manifest.is_file(),
        "robot project is missing root Cargo.toml train anchor; run `phoxal init` or add a non-published root package depending on workspace phoxal"
    );
    ensure!(
        lock.is_file(),
        "robot project is missing committed Cargo.lock; run `cargo generate-lockfile`, review it, and commit it before project-bound commands"
    );

    let output = Command::new("cargo")
        .args(["metadata", "--locked", "--format-version", "1"])
        .current_dir(project_root)
        .output()
        .context("Cargo is required to resolve the locked framework train; install a compatible Rust toolchain")?;
    if !output.status.success() {
        bail!(
            "Cargo.lock is stale or the root train anchor is invalid; project commands never update it automatically. Run `cargo check --locked` or deliberately bump with `cargo update -p phoxal`: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let metadata: Metadata =
        serde_json::from_slice(&output.stdout).context("cargo metadata returned malformed JSON")?;
    let root_manifest = manifest
        .canonicalize()
        .with_context(|| format!("failed to canonicalize {}", manifest.display()))?;
    let root_package = metadata
        .packages
        .iter()
        .find(|package| {
            Path::new(&package.manifest_path)
                .canonicalize()
                .is_ok_and(|path| path == root_manifest)
        })
        .context("root Cargo.toml must define the non-published train-anchor package")?;
    ensure!(
        root_package.publish.as_ref().is_some_and(Vec::is_empty),
        "root train-anchor package must set publish = false"
    );
    ensure!(
        root_package
            .dependencies
            .iter()
            .any(|dependency| dependency.name == "phoxal"),
        "root train-anchor package must depend on phoxal so Cargo.lock selects the train"
    );
    ensure!(
        metadata.workspace_members.contains(&root_package.id),
        "root train-anchor package must be a member of the robot Cargo workspace"
    );
    let packages = metadata
        .packages
        .into_iter()
        .filter(|package| package.name == "phoxal")
        .collect::<Vec<_>>();
    ensure!(
        packages.len() == 1,
        "locked Cargo graph must contain exactly one phoxal package, found {}",
        packages.len()
    );
    let package = &packages[0];
    let source = match package.source.as_deref() {
        Some(source) if source.starts_with("registry+") => TrainSource::Registry,
        Some(source) if source.starts_with("git+") => TrainSource::Git(source.to_string()),
        Some(source) => bail!("unsupported locked phoxal source {source}"),
        None => TrainSource::Path,
    };
    Ok(LockedTrain {
        version: package.version.clone(),
        source,
    })
}

/// Inspect the exact public facade version without selecting or updating it.
pub fn inspect_registry_train(version: &str) -> Result<RegistryStatus> {
    let url = format!("https://crates.io/api/v1/crates/phoxal/{version}");
    let response = reqwest::blocking::Client::builder()
        .user_agent("phoxal-cli")
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(5))
        .build()?
        .get(&url)
        .send()
        .with_context(|| format!("failed to inspect crates.io facade surface {url}"))?;
    let status = response.status();
    if status == reqwest::StatusCode::NOT_FOUND {
        bail!(
            "framework train {version} is not yet published on the crates.io phoxal facade surface ({url}); retry after publication completes"
        );
    }
    ensure!(
        status.is_success(),
        "crates.io facade surface {url} returned HTTP {status}; retry without changing Cargo.lock"
    );
    parse_registry_status(&response.text()?)
}

fn parse_registry_status(body: &str) -> Result<RegistryStatus> {
    #[derive(Deserialize)]
    struct Response {
        version: Version,
    }
    #[derive(Deserialize)]
    struct Version {
        yanked: bool,
    }
    let response: Response =
        serde_json::from_str(body).context("crates.io returned malformed version metadata")?;
    Ok(if response.version.yanked {
        RegistryStatus::Yanked
    } else {
        RegistryStatus::Available
    })
}

#[derive(Deserialize)]
struct Metadata {
    packages: Vec<Package>,
    workspace_members: Vec<String>,
}

#[derive(Deserialize)]
struct Package {
    id: String,
    name: String,
    version: String,
    source: Option<String>,
    manifest_path: String,
    publish: Option<Vec<String>>,
    dependencies: Vec<PackageDependency>,
}

#[derive(Deserialize)]
struct PackageDependency {
    name: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn registry_metadata_distinguishes_yanked_locked_train() {
        assert_eq!(
            parse_registry_status(r#"{"version":{"yanked":true}}"#).unwrap(),
            RegistryStatus::Yanked
        );
        assert_eq!(
            parse_registry_status(r#"{"version":{"yanked":false}}"#).unwrap(),
            RegistryStatus::Available
        );
    }

    #[test]
    fn missing_lock_is_actionable_and_never_created() {
        let root = tempfile::tempdir().unwrap();
        fs::write(
            root.path().join("Cargo.toml"),
            "[package]\nname='anchor'\nversion='0.0.0'\nedition='2024'\n",
        )
        .unwrap();
        let error = resolve_locked_train(root.path()).unwrap_err();
        assert!(error.to_string().contains("missing committed Cargo.lock"));
        assert!(!root.path().join("Cargo.lock").exists());
    }

    #[test]
    fn stale_lock_is_not_rewritten() {
        let root = tempfile::tempdir().unwrap();
        fs::write(
            root.path().join("Cargo.toml"),
            "[package]\nname='anchor'\nversion='0.0.0'\nedition='2024'\n[dependencies]\nanyhow='1'\n",
        )
        .unwrap();
        let lock = "# deliberately stale\nversion = 4\n";
        fs::write(root.path().join("Cargo.lock"), lock).unwrap();
        let error = resolve_locked_train(root.path()).unwrap_err();
        assert!(error.to_string().contains("never update it automatically"));
        assert_eq!(
            fs::read_to_string(root.path().join("Cargo.lock")).unwrap(),
            lock
        );
    }
}
