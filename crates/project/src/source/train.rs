//! Exact framework-train resolution from an authored project's locked graph.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail, ensure};
use phoxal::version::FrameworkVersion;
use serde::Deserialize;

/// The framework a robot project selects, read from its committed Cargo graph.
///
/// The project picks the framework; nothing else does. The `phoxal` version
/// its lockfile resolves is therefore the authority every build-time
/// compatibility decision is made against: a participant binary belongs to
/// this project exactly when the train it was built from shares a
/// compatibility line with [`Self::framework`].
///
/// The exact version is kept alongside that identity because the two answer
/// different questions. The identity decides what interoperates; the exact
/// version is the provenance a diagnostic names and the pin package resolution
/// installs (`<package>@<exact>`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LockedTrain {
    version: String,
    framework: FrameworkVersion,
}

impl LockedTrain {
    /// The project target for the `phoxal` version a lockfile resolved.
    ///
    /// A version that is not a canonical framework version is refused here
    /// rather than carried: such a project selects no framework at all, so
    /// every later participant check would have nothing to compare a binary
    /// against.
    pub fn from_locked_version(version: &str) -> Result<Self> {
        let framework = version.parse::<FrameworkVersion>().with_context(|| {
            format!(
                "the locked `phoxal` version `{version}` is not a canonical \
                 <major>.<minor>.<patch> framework version, so this project selects no framework \
                 to build against; depend on a released `phoxal` version and commit the resulting \
                 lockfile"
            )
        })?;
        Ok(Self {
            version: version.to_string(),
            framework,
        })
    }

    /// The exact locked version, for provenance and package resolution.
    #[must_use]
    pub fn version(&self) -> &str {
        &self.version
    }

    /// The compatibility identity this project targets. Every participant
    /// binary the project builds or stages is validated against the line this
    /// version belongs to.
    #[must_use]
    pub const fn framework(&self) -> FrameworkVersion {
        self.framework
    }
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

/// The root Cargo package, validated as the project's one mandatory brain
/// source.
///
/// Cargo metadata is the sole authority here: the target count and the bin
/// target name come from the already-loaded `cargo metadata --locked` graph,
/// never from `[[bin]]` parsing, package naming, or directory naming.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RootBrainPackage {
    pub package: String,
    pub crate_dir: PathBuf,
    pub bin_target: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LockedProject {
    pub train: LockedTrain,
    /// The root package resolved as the mandatory brain source. Not optional:
    /// a project whose root cannot be a brain fails resolution before any
    /// build.
    pub brain: RootBrainPackage,
    pub runtimes: Vec<WorkspaceRuntime>,
    pub local_components: Vec<WorkspaceComponentCrate>,
}

/// `offline` passes `--offline` to the underlying `cargo metadata`
/// because `PHOXAL_OFFLINE` is a
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
        "robot project is missing its root Cargo.toml; add the non-published root brain package depending on workspace phoxal and commit its Cargo.lock"
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
        .context("root Cargo.toml must define the non-published root brain package")?;
    ensure!(
        root_package.publish.as_ref().is_some_and(Vec::is_empty),
        "root brain package must set publish = false"
    );
    ensure!(
        root_package
            .dependencies
            .iter()
            .any(|dependency| dependency.name == "phoxal"),
        "root brain package must depend on phoxal so Cargo.lock selects the train"
    );
    ensure!(
        metadata.workspace_members.contains(&root_package.id),
        "root brain package must be a member of the robot Cargo workspace"
    );
    let brain = resolve_root_brain(&root_manifest, root_package)?;
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
    let train = LockedTrain::from_locked_version(&package.version).with_context(|| {
        format!(
            "failed to read the framework target from {}",
            lock.display()
        )
    })?;
    let runtimes = discover_workspace_runtimes(project_root, &metadata)?;
    let local_components = discover_local_component_packages(project_root, &metadata)?;
    Ok(LockedProject {
        train,
        brain,
        runtimes,
        local_components,
    })
}

/// Validate the root Cargo package as the project's mandatory brain source
/// and return its exact Cargo-metadata-reported shape.
///
/// The root's publish/workspace-membership/`phoxal`-dependency invariants are
/// already proven by the caller; what this adds is the executable half: Cargo
/// metadata must report exactly one binary target (auto-discovered
/// `src/main.rs` and `src/bin/*` targets included) and no library target.
fn resolve_root_brain(root_manifest: &Path, root_package: &Package) -> Result<RootBrainPackage> {
    let crate_dir = root_manifest
        .parent()
        .context("root Cargo.toml has no parent directory")?
        .to_path_buf();
    let mut binary_names = root_package
        .targets
        .iter()
        .filter(|target| target.kind.iter().any(|kind| kind == "bin"))
        .map(|target| target.name.clone())
        .collect::<Vec<_>>();
    binary_names.sort();
    binary_names.dedup();
    let has_library = root_package.targets.iter().any(|target| {
        target.kind.iter().any(|kind| {
            matches!(
                kind.as_str(),
                "lib" | "rlib" | "dylib" | "cdylib" | "staticlib" | "proc-macro"
            )
        })
    });
    ensure!(
        !has_library,
        "root brain package `{}` must not define a library target; the root package is the \
         robot's one brain binary (src/main.rs with #[phoxal::brain]) and nothing else",
        root_package.name
    );
    ensure!(
        binary_names.len() == 1,
        "root brain package `{}` must define exactly one binary target, found {} ({}); Cargo \
         auto-discovers src/main.rs and every src/bin/* target, so remove the extra ones",
        root_package.name,
        binary_names.len(),
        if binary_names.is_empty() {
            "none".to_string()
        } else {
            binary_names.join(", ")
        }
    );
    Ok(RootBrainPackage {
        package: root_package.name.clone(),
        crate_dir,
        bin_target: binary_names.remove(0),
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
            "Cargo.lock is stale or the root brain package is invalid; project commands never update it automatically. Run `cargo check --locked` or deliberately bump with `cargo update -p phoxal`: {}",
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
        // their own resolution path, and the tool concept is gone.
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

    /// The project target is exactly what the lockfile selected: the precise
    /// version for provenance, and the line it belongs to for compatibility.
    #[test]
    fn the_project_target_is_the_locked_version_and_the_line_it_belongs_to() {
        let train = LockedTrain::from_locked_version("0.42.3")
            .expect("a canonical locked version yields a project target");
        assert_eq!(train.version(), "0.42.3");
        assert_eq!(train.framework(), FrameworkVersion::new(0, 42, 3));
        assert_eq!(train.framework().compatibility_line().to_string(), "0.42.x");
        assert!(
            train
                .framework()
                .is_compatible_with(FrameworkVersion::new(0, 42, 9))
        );
        assert!(
            !train
                .framework()
                .is_compatible_with(FrameworkVersion::new(0, 43, 0))
        );
    }

    /// A `phoxal` version that is not a framework version leaves the project
    /// with nothing to build against, so it is refused where it is read - and
    /// the resolution failure names the lockfile the version came from.
    #[test]
    fn a_locked_phoxal_version_that_is_not_a_framework_version_is_refused() {
        for unsupported in ["0.42", "v0.42.3", "0.42.3-rc.1"] {
            let message = format!(
                "{:#}",
                LockedTrain::from_locked_version(unsupported)
                    .expect_err("a non-canonical locked version has no compatibility identity")
            );
            assert!(message.contains(unsupported), "{message}");
            assert!(message.contains("selects no framework"), "{message}");
            assert!(
                message.contains("commit the resulting lockfile"),
                "{message}"
            );
        }
    }

    /// A robot project never declares or resolves a CLI compatibility
    /// requirement. The framework it targets comes from its own lockfile, and
    /// this crate keeps no second authority to fall back on - not the CLI
    /// product version, and not the framework train the CLI happens to link.
    ///
    /// The rule is textual because it is about what this crate may read at
    /// all, and it covers fixtures too: a test that reached for the CLI's own
    /// train would quietly reintroduce exactly the dependency being removed.
    #[test]
    fn the_project_crate_never_reads_the_cli_as_a_compatibility_input() {
        // Assembled from parts so this policy test cannot match itself.
        let forbidden = [
            ["FrameworkVersion::", "CURRENT"].concat(),
            ["CURRENT_", "SPELLING"].concat(),
            ["CARGO_PKG_", "VERSION"].concat(),
        ];
        let mut offences = Vec::new();
        let mut pending = vec![PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src")];
        while let Some(directory) = pending.pop() {
            for entry in fs::read_dir(&directory).expect("the crate source tree is readable") {
                let path = entry.expect("a readable directory entry").path();
                if path.is_dir() {
                    pending.push(path);
                    continue;
                }
                if path.extension().and_then(|extension| extension.to_str()) != Some("rs") {
                    continue;
                }
                let source = fs::read_to_string(&path).expect("a readable Rust source file");
                for (index, line) in source.lines().enumerate() {
                    if forbidden
                        .iter()
                        .any(|needle| line.contains(needle.as_str()))
                    {
                        offences.push(format!("{}:{}: {}", path.display(), index + 1, line.trim()));
                    }
                }
            }
        }
        assert!(
            offences.is_empty(),
            "the robot project's framework comes from its own Cargo.lock; these read the CLI \
             instead:\n{}",
            offences.join("\n")
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

    /// Cargo metadata, never `[[bin]]` parsing or directory naming, decides
    /// the root brain's target shape.
    fn root_package(targets: Vec<Target>) -> Package {
        Package {
            id: "root".to_string(),
            name: "testbot-robot".to_string(),
            version: "0.1.0".to_string(),
            manifest_path: "/robot/Cargo.toml".to_string(),
            publish: Some(Vec::new()),
            dependencies: Vec::new(),
            targets,
        }
    }

    fn target(name: &str, kind: &str) -> Target {
        Target {
            name: name.to_string(),
            kind: vec![kind.to_string()],
        }
    }

    #[test]
    fn the_root_brain_is_its_exact_metadata_bin_target_never_its_package_name() {
        let brain = resolve_root_brain(
            Path::new("/robot/Cargo.toml"),
            &root_package(vec![target("testbot-robot", "bin")]),
        )
        .expect("a bin-only root package is the brain");
        assert_eq!(brain.package, "testbot-robot");
        assert_eq!(brain.bin_target, "testbot-robot");
        assert_eq!(brain.crate_dir, Path::new("/robot"));

        // The bin target need not match the package name at all; the canonical
        // runtime identity `brain` is never derived from either.
        let renamed = resolve_root_brain(
            Path::new("/robot/Cargo.toml"),
            &root_package(vec![target("rover-brain", "bin")]),
        )
        .expect("a project-specific bin target name is legal");
        assert_eq!(renamed.bin_target, "rover-brain");
    }

    #[test]
    fn a_root_with_a_library_or_the_wrong_number_of_bins_is_rejected() {
        // A binary AND a library: the root package is the brain binary and
        // nothing else.
        let error = resolve_root_brain(
            Path::new("/robot/Cargo.toml"),
            &root_package(vec![
                target("testbot-robot", "bin"),
                target("testbot_robot", "lib"),
            ]),
        )
        .expect_err("a root that also exports a library must be rejected")
        .to_string();
        assert!(
            error.contains("must not define a library target"),
            "{error}"
        );

        // A stray `src/bin/*` target: Cargo auto-discovers it, so the brain
        // would be ambiguous.
        let error = resolve_root_brain(
            Path::new("/robot/Cargo.toml"),
            &root_package(vec![
                target("testbot-robot", "bin"),
                target("scratch", "bin"),
            ]),
        )
        .expect_err("two bin targets make the brain ambiguous")
        .to_string();
        assert!(error.contains("exactly one binary target"), "{error}");
        assert!(error.contains("scratch"), "{error}");

        // No target at all.
        let error = resolve_root_brain(Path::new("/robot/Cargo.toml"), &root_package(Vec::new()))
            .expect_err("a root with no binary cannot be the brain")
            .to_string();
        assert!(error.contains("exactly one binary target"), "{error}");
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
        // `tools/` is no longer a runtime family, and
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
