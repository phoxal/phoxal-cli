//! Exact framework-train resolution from a robot project's locked Cargo graph.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkspaceRuntimeKind {
    Service,
    Tool,
    Component,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceRuntime {
    pub package: String,
    pub crate_dir: PathBuf,
    pub kind: WorkspaceRuntimeKind,
    pub binary_names: Vec<String>,
    /// Directory containing `component.yaml`; present for every component
    /// workspace package and absent for other runtime kinds.
    pub component_assets: Option<PathBuf>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LockedProject {
    pub train: LockedTrain,
    pub runtimes: Vec<WorkspaceRuntime>,
}

impl LockedTrain {
    #[must_use]
    pub fn is_published(&self) -> bool {
        matches!(self.source, TrainSource::Registry)
    }
}

/// `offline` passes `--offline` to the underlying `cargo metadata`
/// (organization#951 WS4 review, medium 4): `PHOXAL_OFFLINE` is a
/// Phoxal-only env var Cargo does not read, so a caller that wants a
/// genuinely offline resolution must set this explicitly.
pub fn resolve_locked_train(project_root: &Path, offline: bool) -> Result<LockedTrain> {
    Ok(resolve_locked_project(project_root, offline)?.train)
}

/// Resolve the framework train and every user runtime from the same immutable
/// `cargo metadata --locked` graph. See [`resolve_locked_train`] for
/// `offline`.
pub fn resolve_locked_project(project_root: &Path, offline: bool) -> Result<LockedProject> {
    let manifest = project_root.join("Cargo.toml");
    let lock = project_root.join("Cargo.lock");
    ensure!(
        manifest.is_file(),
        "robot project is missing root Cargo.toml train anchor; add a non-published root package depending on workspace phoxal and commit its Cargo.lock"
    );
    ensure!(
        lock.is_file(),
        "robot project is missing committed Cargo.lock; run `cargo generate-lockfile`, review it, and commit it before project-bound commands"
    );

    let metadata = load_metadata(project_root, offline)?;
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
        .iter()
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
    let train = LockedTrain {
        version: package.version.clone(),
        source,
    };
    let runtimes = discover_workspace_runtimes(project_root, &metadata)?;
    Ok(LockedProject { train, runtimes })
}

fn load_metadata(project_root: &Path, offline: bool) -> Result<Metadata> {
    let mut command = Command::new("cargo");
    command
        .args(["metadata", "--locked", "--format-version", "1"])
        .current_dir(project_root);
    if offline {
        command.arg("--offline");
    }
    let output = command
        .output()
        .context("Cargo is required to resolve the locked framework train; install a compatible Rust toolchain")?;
    if !output.status.success() {
        bail!(
            "Cargo.lock is stale or the root train anchor is invalid; project commands never update it automatically. Run `cargo check --locked` or deliberately bump with `cargo update -p phoxal`: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    serde_json::from_slice(&output.stdout).context("cargo metadata returned malformed JSON")
}

fn discover_workspace_runtimes(
    project_root: &Path,
    metadata: &Metadata,
) -> Result<Vec<WorkspaceRuntime>> {
    let root = project_root
        .canonicalize()
        .with_context(|| format!("failed to canonicalize {}", project_root.display()))?;
    let members = metadata.workspace_members.iter().collect::<BTreeSet<_>>();
    let packages = metadata
        .packages
        .iter()
        .map(|package| (package.id.as_str(), package))
        .collect::<BTreeMap<_, _>>();
    let direct_dependencies = metadata
        .resolve
        .as_ref()
        .map(|resolve| {
            resolve
                .nodes
                .iter()
                .map(|node| {
                    (
                        node.id.as_str(),
                        node.deps
                            .iter()
                            .map(|dependency| dependency.pkg.as_str())
                            .collect::<Vec<_>>(),
                    )
                })
                .collect::<BTreeMap<_, _>>()
        })
        .unwrap_or_default();
    let mut runtimes = Vec::new();
    for package in metadata
        .packages
        .iter()
        .filter(|package| members.contains(&package.id))
    {
        let manifest = Path::new(&package.manifest_path)
            .canonicalize()
            .with_context(|| format!("failed to canonicalize {}", package.manifest_path))?;
        let crate_dir = manifest
            .parent()
            .context("Cargo package manifest has no parent")?
            .to_path_buf();
        let relative = match crate_dir.strip_prefix(&root) {
            Ok(relative) => relative,
            Err(_) => continue,
        };
        let Some(directory) = relative
            .components()
            .next()
            .and_then(|part| part.as_os_str().to_str())
        else {
            continue;
        };
        let kind = match directory {
            "services" => WorkspaceRuntimeKind::Service,
            "tools" => WorkspaceRuntimeKind::Tool,
            "components" => WorkspaceRuntimeKind::Component,
            _ => continue,
        };
        let mut binary_names = package
            .targets
            .iter()
            .filter(|target| target.kind.iter().any(|kind| kind == "bin"))
            .map(|target| target.name.clone())
            .collect::<Vec<_>>();
        binary_names.sort();
        binary_names.dedup();
        if kind != WorkspaceRuntimeKind::Component && binary_names.len() != 1 {
            bail!(
                "{} workspace package {} must define exactly one bin target, found {}",
                directory,
                package.name,
                binary_names.len()
            );
        }
        if kind == WorkspaceRuntimeKind::Component && binary_names.len() > 1 {
            bail!(
                "component workspace package {} must define at most one bin target, found {}",
                package.name,
                binary_names.len()
            );
        }
        let component_assets = if kind == WorkspaceRuntimeKind::Component {
            let mut definitions = Vec::new();
            if crate_dir.join("component.yaml").is_file() {
                definitions.push(crate_dir.clone());
            }
            for dependency in direct_dependencies
                .get(package.id.as_str())
                .into_iter()
                .flatten()
            {
                let Some(dependency) = packages.get(dependency) else {
                    continue;
                };
                let directory = Path::new(&dependency.manifest_path)
                    .parent()
                    .context("dependency manifest has no parent")?;
                if directory.join("component.yaml").is_file() {
                    definitions.push(directory.to_path_buf());
                }
            }
            definitions.sort();
            definitions.dedup();
            if definitions.len() != 1 {
                bail!(
                    "component workspace package {} must resolve component.yaml from itself or exactly one direct dependency, found {}",
                    package.name,
                    definitions.len()
                );
            }
            definitions.pop()
        } else {
            None
        };
        runtimes.push(WorkspaceRuntime {
            package: package.name.clone(),
            crate_dir,
            kind,
            binary_names,
            component_assets,
        });
    }
    runtimes.sort_by(|left, right| left.crate_dir.cmp(&right.crate_dir));
    Ok(runtimes)
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
    resolve: Option<Resolve>,
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
    targets: Vec<Target>,
}

#[derive(Deserialize)]
struct PackageDependency {
    name: String,
}

#[derive(Deserialize)]
struct Target {
    name: String,
    kind: Vec<String>,
}

#[derive(Deserialize)]
struct Resolve {
    nodes: Vec<ResolveNode>,
}

#[derive(Deserialize)]
struct ResolveNode {
    id: String,
    deps: Vec<ResolveDependency>,
}

#[derive(Deserialize)]
struct ResolveDependency {
    pkg: String,
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
        let error = resolve_locked_train(root.path(), false).unwrap_err();
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
        let error = resolve_locked_train(root.path(), false).unwrap_err();
        assert!(error.to_string().contains("never update it automatically"));
        assert_eq!(
            fs::read_to_string(root.path().join("Cargo.lock")).unwrap(),
            lock
        );
    }

    #[test]
    fn workspace_runtime_discovery_classifies_directories_and_component_assets() {
        let root = tempfile::tempdir().unwrap();
        let write_manifest = |relative: &str| {
            let path = root.path().join(relative).join("Cargo.toml");
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, "").unwrap();
        };
        for directory in [
            "services/mission",
            "tools/operator",
            "components/passive",
            "components/wrapped",
            "vendor/remote-component",
        ] {
            write_manifest(directory);
        }
        fs::write(
            root.path().join("components/passive/component.yaml"),
            "schema: component/v0\n",
        )
        .unwrap();
        fs::write(
            root.path().join("vendor/remote-component/component.yaml"),
            "schema: component/v0\n",
        )
        .unwrap();

        let package = |id: &str, name: &str, directory: &str, binaries: &[&str]| Package {
            id: id.to_string(),
            name: name.to_string(),
            version: "0.1.0".to_string(),
            source: None,
            manifest_path: root
                .path()
                .join(directory)
                .join("Cargo.toml")
                .display()
                .to_string(),
            publish: None,
            dependencies: Vec::new(),
            targets: binaries
                .iter()
                .map(|name| Target {
                    name: (*name).to_string(),
                    kind: vec!["bin".to_string()],
                })
                .collect(),
        };
        let metadata = Metadata {
            packages: vec![
                package("mission", "mission", "services/mission", &["mission"]),
                package("operator", "operator", "tools/operator", &["operator"]),
                package("passive", "passive", "components/passive", &[]),
                package("wrapped", "wrapped", "components/wrapped", &["wrapped"]),
                package(
                    "remote-component",
                    "remote-component",
                    "vendor/remote-component",
                    &[],
                ),
            ],
            workspace_members: vec![
                "mission".to_string(),
                "operator".to_string(),
                "passive".to_string(),
                "wrapped".to_string(),
            ],
            resolve: Some(Resolve {
                nodes: vec![ResolveNode {
                    id: "wrapped".to_string(),
                    deps: vec![ResolveDependency {
                        pkg: "remote-component".to_string(),
                    }],
                }],
            }),
        };

        let runtimes = discover_workspace_runtimes(root.path(), &metadata).unwrap();
        assert_eq!(runtimes.len(), 4);
        let runtime = |package: &str| {
            runtimes
                .iter()
                .find(|runtime| runtime.package == package)
                .unwrap()
        };
        assert_eq!(runtime("mission").kind, WorkspaceRuntimeKind::Service);
        assert_eq!(runtime("mission").binary_names, ["mission"]);
        assert_eq!(runtime("operator").kind, WorkspaceRuntimeKind::Tool);
        assert_eq!(runtime("operator").binary_names, ["operator"]);
        assert_eq!(runtime("passive").kind, WorkspaceRuntimeKind::Component);
        assert!(runtime("passive").binary_names.is_empty());
        assert_eq!(
            runtime("passive").component_assets.as_deref(),
            Some(
                root.path()
                    .join("components/passive")
                    .canonicalize()
                    .unwrap()
                    .as_path()
            )
        );
        assert_eq!(runtime("wrapped").kind, WorkspaceRuntimeKind::Component);
        assert_eq!(runtime("wrapped").binary_names, ["wrapped"]);
        assert_eq!(
            runtime("wrapped").component_assets.as_deref(),
            Some(root.path().join("vendor/remote-component").as_path())
        );
    }
}
