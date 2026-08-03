//! Exact framework-train resolution from a robot project's locked Cargo graph.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceRuntime {
    pub package: String,
    pub crate_dir: PathBuf,
    pub binary_names: Vec<String>,
}

/// A `components/<id>/Cargo.toml` package retained from the root locked graph.
/// Component discovery is filesystem-based, while this record supplies the
/// already-locked Cargo shape without launching one metadata process per
/// component.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceComponentCrate {
    pub manifest_path: PathBuf,
    pub crate_dir: PathBuf,
    pub binary_names: Vec<String>,
    pub has_library: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LockedProject {
    pub train: LockedTrain,
    pub runtimes: Vec<WorkspaceRuntime>,
    pub local_components: Vec<WorkspaceComponentCrate>,
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
    let local_components = discover_local_component_packages(project_root, &metadata)?;
    Ok(LockedProject {
        train,
        runtimes,
        local_components,
    })
}

fn discover_local_component_packages(
    project_root: &Path,
    metadata: &Metadata,
) -> Result<Vec<WorkspaceComponentCrate>> {
    let root = project_root
        .canonicalize()
        .with_context(|| format!("failed to canonicalize {}", project_root.display()))?;
    let members = metadata.workspace_members.iter().collect::<BTreeSet<_>>();
    let mut components = Vec::new();
    for package in metadata
        .packages
        .iter()
        .filter(|package| members.contains(&package.id))
    {
        let manifest_path = Path::new(&package.manifest_path)
            .canonicalize()
            .with_context(|| format!("failed to canonicalize {}", package.manifest_path))?;
        let crate_dir = manifest_path
            .parent()
            .context("Cargo package manifest has no parent")?
            .to_path_buf();
        let Ok(relative) = crate_dir.strip_prefix(&root) else {
            continue;
        };
        if relative
            .components()
            .next()
            .and_then(|part| part.as_os_str().to_str())
            != Some("components")
        {
            continue;
        }
        let mut binary_names = package
            .targets
            .iter()
            .filter(|target| target.kind.iter().any(|kind| kind == "bin"))
            .map(|target| target.name.clone())
            .collect::<Vec<_>>();
        binary_names.sort();
        binary_names.dedup();
        let has_library = package.targets.iter().any(|target| {
            target.kind.iter().any(|kind| {
                matches!(
                    kind.as_str(),
                    "lib" | "rlib" | "dylib" | "cdylib" | "staticlib" | "proc-macro"
                )
            })
        });
        components.push(WorkspaceComponentCrate {
            manifest_path,
            crate_dir,
            binary_names,
            has_library,
        });
    }
    components.sort_by(|left, right| left.crate_dir.cmp(&right.crate_dir));
    Ok(components)
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
        // Only `services/` carries workspace runtime crates: components have
        // their own resolution path, and the tool concept is gone
        // (organization#978).
        if directory != "services" {
            continue;
        }
        let mut binary_names = package
            .targets
            .iter()
            .filter(|target| target.kind.iter().any(|kind| kind == "bin"))
            .map(|target| target.name.clone())
            .collect::<Vec<_>>();
        binary_names.sort();
        binary_names.dedup();
        if binary_names.len() != 1 {
            bail!(
                "{} workspace package {} must define exactly one bin target, found {}",
                directory,
                package.name,
                binary_names.len()
            );
        }
        runtimes.push(WorkspaceRuntime {
            package: package.name.clone(),
            crate_dir,
            binary_names,
        });
    }
    runtimes.sort_by(|left, right| left.crate_dir.cmp(&right.crate_dir));
    Ok(runtimes)
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

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
    fn workspace_runtime_discovery_classifies_services_and_tools_and_ignores_components() {
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
        ] {
            write_manifest(directory);
        }

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
            ],
            workspace_members: vec![
                "mission".to_string(),
                "operator".to_string(),
                "passive".to_string(),
                "wrapped".to_string(),
            ],
        };

        let runtimes = discover_workspace_runtimes(root.path(), &metadata).unwrap();
        assert_eq!(
            runtimes.len(),
            1,
            "only services/ carries workspace runtimes"
        );
        assert_eq!(runtimes[0].package, "mission");
        assert_eq!(runtimes[0].binary_names, ["mission"]);
        // `tools/` is no longer a runtime family (organization#978), and
        // `components/` never was one.
        for ignored in ["operator", "passive", "wrapped"] {
            assert!(
                runtimes.iter().all(|runtime| runtime.package != ignored),
                "{ignored} must not be discovered as a workspace runtime"
            );
        }
    }

    #[test]
    fn locked_metadata_retains_component_driver_target_shapes() {
        let root = tempfile::tempdir().unwrap();
        let package = |id: &str, directory: &str, targets: Vec<Target>| {
            let manifest = root.path().join(directory).join("Cargo.toml");
            fs::create_dir_all(manifest.parent().unwrap()).unwrap();
            fs::write(&manifest, "").unwrap();
            Package {
                id: id.to_string(),
                name: id.to_string(),
                version: "0.1.0".to_string(),
                source: None,
                manifest_path: manifest.display().to_string(),
                publish: None,
                dependencies: Vec::new(),
                targets,
            }
        };
        let target = |name: &str, kinds: &[&str]| Target {
            name: name.to_string(),
            kind: kinds.iter().map(|kind| (*kind).to_string()).collect(),
        };
        let metadata = Metadata {
            packages: vec![
                package("one", "components/one", vec![target("one", &["bin"])]),
                package(
                    "mixed",
                    "components/mixed",
                    vec![target("mixed", &["bin"]), target("mixed", &["lib"])],
                ),
                package(
                    "many",
                    "components/many",
                    vec![target("many", &["bin"]), target("extra", &["bin"])],
                ),
                // This package has the same components-root shape as the
                // members above but is only a transitive metadata package,
                // never a root workspace component.
                package(
                    "nonmember",
                    "components/nonmember",
                    vec![target("nonmember", &["bin"])],
                ),
            ],
            workspace_members: vec!["one".into(), "mixed".into(), "many".into()],
        };
        let components = discover_local_component_packages(root.path(), &metadata).unwrap();
        assert_eq!(components.len(), 3);
        assert_eq!(components[0].binary_names, ["extra", "many"]);
        assert!(!components[0].has_library);
        assert_eq!(components[1].binary_names, ["mixed"]);
        assert!(components[1].has_library);
        assert_eq!(components[2].binary_names, ["one"]);
        assert!(!components[2].has_library);
        assert!(
            components
                .iter()
                .all(|component| component.crate_dir != root.path().join("components/nonmember"))
        );
    }
}
