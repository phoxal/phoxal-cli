use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

pub use crate::source::host_target_triple;
use anyhow::{Context, Result, anyhow};
use phoxal::authoring::source::robot::v0::Manifest as Robot;
use phoxal_cli_catalog::{ArtifactKind, Catalog, OfficialRuntime};

/// The provider every official Phoxal package uses in catalog identities.
const PHOXAL_PROVIDER: &str = "phoxal";

use crate::source::resolver::{
    BundlePlan, CompiledBundle, ResolveOptions, ResolvedBrain, ResolvedComponent,
    ResolvedComponentDriver, ResolvedPathOverride, ResolvedPlatformRuntime, ResolvedUserRuntime,
    UndeclaredRuntime,
};

/// Resolve a robot manifest against the CLI-internal official catalog
/// with no suite fetch or vendored artifact store. Every
/// official service and every component
/// driver package materializes later via `cargo install` at exactly the
/// locked framework train; component assets resolve directly from authored
/// directories or the checked sparse package cache, so the manifest compiler
/// receives the exact source root without learning Cargo or registry policy.
#[cfg(test)]
pub fn resolve(robot: &Robot, project_root: &Path, options: ResolveOptions) -> Result<BundlePlan> {
    resolve_with_train(robot, project_root, options, |_| {})
}

pub(crate) fn resolve_with_train(
    robot: &Robot,
    project_root: &Path,
    options: ResolveOptions,
    resolved_train: impl FnOnce(&str),
) -> Result<BundlePlan> {
    resolve_with_train_using_registry_cache(
        robot,
        project_root,
        &project_root.join(".phoxal/cache/registry"),
        options,
        resolved_train,
    )
}

/// Resolve a frozen source tree while keeping immutable registry archives in
/// the live project's operational cache. Container snapshots are source
/// inputs, never cache owners.
pub(crate) fn resolve_with_train_using_registry_cache(
    robot: &Robot,
    project_root: &Path,
    registry_cache_root: &Path,
    options: ResolveOptions,
    resolved_train: impl FnOnce(&str),
) -> Result<BundlePlan> {
    let project = crate::source::train::resolve_locked_project(project_root, options.offline)?;
    resolved_train(project.train.version());
    resolve_with_locked_project_using_registry_cache(
        robot,
        project_root,
        registry_cache_root,
        options,
        &project,
    )
}

/// Resolve against a locked workspace the caller has already loaded. `check`
/// uses this entry so structural workspace reporting and canonical compilation
/// share one `cargo metadata --locked` result.
pub(crate) fn resolve_with_locked_project(
    robot: &Robot,
    project_root: &Path,
    options: ResolveOptions,
    project: &crate::source::train::LockedProject,
) -> Result<BundlePlan> {
    resolve_with_locked_project_using_registry_cache(
        robot,
        project_root,
        &project_root.join(".phoxal/cache/registry"),
        options,
        project,
    )
}

pub(crate) fn resolve_with_locked_project_using_registry_cache(
    robot: &Robot,
    project_root: &Path,
    registry_cache_root: &Path,
    options: ResolveOptions,
    project: &crate::source::train::LockedProject,
) -> Result<BundlePlan> {
    // The catalog below is one current snapshot, not per-train history
    // so reject a locked train it
    // predates before applying it, rather than silently resolving an
    // official set that never existed for that train.
    let train = project.train.version().to_string();
    let target = options
        .official_target_triple
        .clone()
        .unwrap_or_else(host_target_triple);
    robot
        .validate()
        .map_err(|errors| anyhow!("Robot errors:\n{}", join_errors(errors)))?;
    let mut platform_runtimes = Catalog::official()
        .native()
        .filter(|official| official.kind == ArtifactKind::Service)
        .map(|official| platform_runtime_from_official(official, &train, Some(&target)))
        .collect::<Vec<_>>();
    let components = resolve_components(
        robot,
        ComponentResolutionContext {
            project_root,
            registry_cache_root,
            train: &train,
            target: &target,
            local_packages: &project.local_components,
            offline: options.offline,
        },
    )?;

    let workspace = apply_workspace_runtimes(
        robot,
        project_root,
        &project.runtimes,
        &mut platform_runtimes,
    )?;

    let component_roots = components
        .iter()
        .map(|component| (component.source_name.clone(), component.assets_root.clone()))
        .collect::<BTreeMap<_, _>>();
    let robot_manifest = project_root.join("robot.yaml");
    let compiled = CompiledBundle::from_project(
        phoxal::authoring::SourceSet {
            project_root: project_root.to_path_buf(),
            robot_manifest,
            component_roots,
        }
        // Which services are official is a build-tooling fact the compiler
        // deliberately does not know, so the catalogue is handed in here rather
        // than duplicated inside the framework.
        .compile(official_service_ids())
        .context("failed to compile the resolved source project")?,
    );

    Ok(BundlePlan {
        source_manifest: robot.clone(),
        compiled,
        train: project.train.clone(),
        target,
        // The root package IS the brain; locked resolution already proved its
        // exact Cargo shape.
        brain: ResolvedBrain {
            crate_dir: project.brain.crate_dir.clone(),
            package: project.brain.package.clone(),
            bin_target: project.brain.bin_target.clone(),
        },
        platform_runtimes,
        user_runtimes: workspace.user_runtimes,
        undeclared_runtimes: workspace.undeclared_runtimes,
        components,
        path_overrides: workspace.path_overrides,
    })
}

fn platform_runtime_from_official(
    official: &OfficialRuntime,
    train: &str,
    target: Option<&str>,
) -> ResolvedPlatformRuntime {
    ResolvedPlatformRuntime {
        name: short_name(official.package, official.kind),
        package: official.package.to_string(),
        kind: official.kind,
        path_override: None,
        train: train.to_string(),
        target: target.map(str::to_string),
    }
}

fn short_name(package: &str, kind: ArtifactKind) -> String {
    let prefix = match kind {
        ArtifactKind::Service => "phoxal/service-",
        ArtifactKind::ComponentDriver => "phoxal/component-",
    };
    package.strip_prefix(prefix).unwrap_or(package).to_string()
}

/// The official service identities the manifest compiler merges into the
/// authored `services:` map.
fn official_service_ids() -> Vec<phoxal::model::identity::ServiceId> {
    Catalog::official()
        .service_identities()
        .into_iter()
        .filter_map(|id| phoxal::model::identity::ServiceId::new(id).ok())
        .collect()
}

/// Resolve authored component roots directly from `components/<id>/` or the
/// exact locked registry package. Cargo workspace membership is deliberately
/// irrelevant to definition discovery.
struct ComponentResolutionContext<'a> {
    project_root: &'a Path,
    registry_cache_root: &'a Path,
    train: &'a str,
    target: &'a str,
    local_packages: &'a [crate::source::train::WorkspaceComponentCrate],
    offline: bool,
}

fn resolve_components(
    robot: &Robot,
    context: ComponentResolutionContext<'_>,
) -> Result<Vec<ResolvedComponent>> {
    let local =
        discover_local_components_from_locked(context.project_root, context.local_packages)?;
    let mut registry_component_ids = BTreeSet::new();
    for instance in robot.robot.components.values() {
        if !local.contains_key(&instance.component) {
            registry_component_ids.insert(instance.component.clone());
        }
    }
    let registry_roots = if registry_component_ids.is_empty() {
        BTreeMap::new()
    } else {
        let http = crate::registry_package::HttpClient::new()?;
        let cache =
            crate::registry_package::PackageCache::new(context.registry_cache_root.to_path_buf());
        resolve_registry_component_roots(
            &registry_component_ids,
            &http,
            &cache,
            context.train,
            context.offline,
        )?
    };

    let mut components = Vec::new();
    for (instance_name, instance) in &robot.robot.components {
        let component_id = &instance.component;
        let package = format!("{PHOXAL_PROVIDER}/component-{component_id}");
        let declares_driver = instance.driver.is_some();

        if let Some(local) = local.get(component_id) {
            let driver = match (&local.driver_crate, declares_driver) {
                (Some(crate_dir), true) => Some(ResolvedComponentDriver::Local {
                    crate_dir: crate_dir.clone(),
                }),
                (None, true) => anyhow::bail!(
                    "robot component instance {instance_name} declares a driver, but local components/{component_id} is asset-only"
                ),
                _ => None,
            };
            components.push(ResolvedComponent {
                instance: instance_name.clone(),
                source_name: component_id.clone(),
                assets_root: local.root.clone(),
                driver,
            });
            continue;
        }

        let assets_root = registry_roots.get(component_id).cloned().ok_or_else(|| {
            anyhow::anyhow!("resolved registry component {component_id} has no materialized root")
        })?;
        let driver = declares_driver.then(|| {
            ResolvedComponentDriver::Registry(ResolvedPlatformRuntime {
                name: component_id.clone(),
                package: package.clone(),
                kind: ArtifactKind::ComponentDriver,
                path_override: None,
                train: context.train.to_string(),
                target: Some(context.target.to_string()),
            })
        });

        components.push(ResolvedComponent {
            instance: instance_name.clone(),
            source_name: component_id.clone(),
            assets_root,
            driver,
        });
    }
    Ok(components)
}

fn resolve_registry_component_roots(
    component_ids: &BTreeSet<String>,
    http: &dyn crate::registry_package::RegistryHttp,
    cache: &crate::registry_package::PackageCache,
    train: &str,
    offline: bool,
) -> Result<BTreeMap<String, std::path::PathBuf>> {
    component_ids
        .iter()
        .map(|id| {
            let package = phoxal_cli_catalog::cargo_package_name(&format!(
                "{PHOXAL_PROVIDER}/component-{id}"
            ));
            let package = crate::registry_package::fetch_registry_package(http, cache, &package, train, offline)
                .with_context(|| format!(
                    "robot component '{id}' failed to resolve {package} from the registry; add a local components/{id}/ directory to override it"
                ))?;
            let driver_source = package.require_component_driver_bin()?;
            let root = package.extracted_root()?;
            package.require_component_driver_source(&root, &driver_source)?;
            Ok((id.clone(), root))
        })
        .collect()
}

#[derive(Debug, Clone)]
struct LocalComponent {
    root: std::path::PathBuf,
    driver_crate: Option<std::path::PathBuf>,
}

fn discover_local_components_from_locked(
    project_root: &Path,
    local_packages: &[crate::source::train::WorkspaceComponentCrate],
) -> Result<BTreeMap<String, LocalComponent>> {
    let canonical_project_root = project_root.canonicalize().with_context(|| {
        format!(
            "failed to canonicalize project root {}",
            project_root.display()
        )
    })?;
    let root = project_root.join("components");
    if !root.exists() {
        return Ok(BTreeMap::new());
    }
    let mut components = BTreeMap::new();
    for entry in
        fs::read_dir(&root).with_context(|| format!("failed to read {}", root.display()))?
    {
        let entry = entry.with_context(|| format!("failed to read {} entry", root.display()))?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let id = entry.file_name().into_string().map_err(|_| {
            anyhow!(
                "component directory has a non-UTF-8 name under {}",
                root.display()
            )
        })?;
        let path = path.canonicalize().with_context(|| {
            format!("failed to canonicalize local component {}", path.display())
        })?;
        anyhow::ensure!(
            path.join("component.yaml").is_file(),
            "local component directory {} is missing component.yaml",
            path.display()
        );
        let manifest = path.join("Cargo.toml");
        let src = path.join("src");
        let driver_crate = if manifest.is_file() {
            if !src.join("main.rs").is_file() {
                if src.join("lib.rs").is_file() {
                    anyhow::bail!(
                        "local component {} is an obsolete anchor/assets crate; delete Cargo.toml and src/, then keep component.yaml and authored assets",
                        path.display()
                    );
                }
                anyhow::bail!(
                    "local driver component {} must use src/main.rs",
                    path.display()
                );
            }
            let package = local_packages
                .iter()
                .find(|package| package.manifest_path == manifest)
                .with_context(|| format!(
                    "local driver component {} must be a member of the root workspace at {}; add it to root workspace.members",
                    manifest.display(),
                    canonical_project_root.display()
                ))?;
            verify_local_driver_shape(package)?;
            Some(path.clone())
        } else {
            anyhow::ensure!(
                !src.exists(),
                "local asset-only component {} must not contain src/ without Cargo.toml",
                path.display()
            );
            None
        };
        components.insert(
            id,
            LocalComponent {
                root: path,
                driver_crate,
            },
        );
    }
    Ok(components)
}

fn verify_local_driver_shape(
    package: &crate::source::train::WorkspaceComponentCrate,
) -> Result<()> {
    anyhow::ensure!(
        package.binary_names.len() == 1,
        "local driver component {} must define exactly one binary target, found {bins}",
        package.manifest_path.display(),
        bins = package.binary_names.len(),
    );
    anyhow::ensure!(
        !package.has_library,
        "local driver component {} must not define a library target",
        package.manifest_path.display()
    );
    Ok(())
}

fn apply_workspace_runtimes(
    robot: &Robot,
    project_root: &Path,
    runtimes: &[crate::source::train::WorkspaceRuntime],
    platform_runtimes: &mut [ResolvedPlatformRuntime],
) -> Result<WorkspaceRuntimeResolution> {
    let mut user_runtimes = Vec::new();
    let mut undeclared = Vec::new();
    let mut overrides = Vec::new();
    for runtime in runtimes {
        let logical_name = runtime
            .crate_dir
            .file_name()
            .and_then(|name| name.to_str())
            .context("runtime crate directory must have a UTF-8 name")?
            .to_string();
        let relative = runtime
            .crate_dir
            .strip_prefix(project_root)
            .unwrap_or(&runtime.crate_dir)
            .to_path_buf();
        let official_package = format!("phoxal/service-{logical_name}");
        if let Some(official) = platform_runtimes
            .iter_mut()
            .find(|entry| entry.package == official_package)
        {
            official.path_override = Some(runtime.crate_dir.clone());
            overrides.push(ResolvedPathOverride {
                key: official_package,
                artifact_name: logical_name,
                path: runtime.crate_dir.clone(),
            });
        } else if robot.services.contains_key(&logical_name) {
            // Declared: the services map selects which discovered
            // workspace services belong to the robot.
            user_runtimes.push(ResolvedUserRuntime {
                name: logical_name,
                path: relative,
            });
        } else {
            // Present but undeclared: legal, not built or launched;
            // surfaced as a drift diagnostic.
            undeclared.push(UndeclaredRuntime { name: logical_name });
        }
    }
    let discovered_services = user_runtimes
        .iter()
        .map(|runtime| runtime.name.as_str())
        .chain(
            overrides
                .iter()
                .map(|override_| override_.artifact_name.as_str()),
        )
        .collect::<std::collections::BTreeSet<_>>();
    // An official identity under `services:` is a configuration entry for a
    // service that runs whether or not the document mentions it, so it owes no
    // workspace crate. A declared official whose source this project DOES
    // override took the path-override branch above and is already discovered.
    let catalog = Catalog::official();
    let missing = robot
        .services
        .keys()
        .filter(|name| !discovered_services.contains(name.as_str()))
        .filter(|name| !catalog.is_official_service(name))
        .collect::<Vec<_>>();
    anyhow::ensure!(
        missing.is_empty(),
        "robot.yaml declares services with no matching services/ workspace crate: {}",
        missing
            .into_iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join(", ")
    );
    overrides.sort_by(|left, right| left.key.cmp(&right.key));
    undeclared.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(WorkspaceRuntimeResolution {
        user_runtimes,
        undeclared_runtimes: undeclared,
        path_overrides: overrides,
    })
}

#[cfg(test)]
fn discover_local_components(
    project_root: &Path,
    offline: bool,
) -> Result<BTreeMap<String, LocalComponent>> {
    let project = crate::source::train::resolve_locked_project(project_root, offline)?;
    discover_local_components_from_locked(project_root, &project.local_components)
}

/// The workspace-runtime half of resolution: the declared user
/// services, the drift records for undeclared crates, and the official-source
/// overrides applied along the way.
#[derive(Debug)]
struct WorkspaceRuntimeResolution {
    user_runtimes: Vec<ResolvedUserRuntime>,
    undeclared_runtimes: Vec<UndeclaredRuntime>,
    path_overrides: Vec<ResolvedPathOverride>,
}

/// Resolve a user-supplied `--target` selector to the full target triple
/// official packages are compiled for. Accepts the short arch aliases
/// (`aarch64`/`arm64`, `x86_64`/`amd64`) or a full triple passed through as-is.
/// Official packages target gnu Linux, so a bare arch maps to the gnu triple.
pub fn resolve_target_triple(selector: &str) -> Result<String> {
    Ok(match selector {
        "aarch64" | "arm64" => "aarch64-unknown-linux-gnu".to_string(),
        "x86_64" | "amd64" => "x86_64-unknown-linux-gnu".to_string(),
        other if other.contains('-') => other.to_string(),
        other => anyhow::bail!(
            "unrecognized --target '{other}'; expected aarch64, x86_64, or a full target triple"
        ),
    })
}

fn join_errors(errors: Vec<phoxal::authoring::source::robot::v0::ValidationError>) -> String {
    errors
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal robot workspace whose locked train resolves to `0.1.0` via a
    /// local path dependency on a stub `phoxal` crate - no registry, no network.
    /// `resolve()` always calls `resolve_locked_project`, so every test needs
    /// this fixture even when it never touches a component.
    fn locked_project_root() -> anyhow::Result<tempfile::TempDir> {
        let root = tempfile::tempdir()?;
        std::fs::create_dir_all(root.path().join("src"))?;
        std::fs::create_dir_all(root.path().join("train/phoxal/src"))?;
        std::fs::create_dir_all(root.path().join("components/fixture/src"))?;
        std::fs::write(
            root.path().join("Cargo.toml"),
            "[package]\nname = \"robot\"\nversion = \"0.1.0\"\nedition = \"2024\"\npublish = false\n\n[workspace]\nmembers = [\".\", \"components/fixture\"]\nresolver = \"2\"\n\n[dependencies]\nphoxal = { path = \"train/phoxal\" }\n",
        )?;
        // The root package IS the mandatory brain: one auto-discovered bin target
        // and no library.
        std::fs::write(root.path().join("src/main.rs"), "fn main() {}")?;
        std::fs::write(
            root.path().join("train/phoxal/Cargo.toml"),
            "[package]\nname = \"phoxal\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        )?;
        std::fs::write(root.path().join("train/phoxal/src/lib.rs"), "")?;
        std::fs::write(
            root.path().join("components/fixture/Cargo.toml"),
            "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[[bin]]\nname = \"fixture\"\npath = \"src/main.rs\"\n",
        )?;
        std::fs::write(
            root.path().join("components/fixture/src/main.rs"),
            "fn main() {}",
        )?;
        std::fs::write(
            root.path().join("components/fixture/component.yaml"),
            "schema: phoxal/component/v0\ncapabilities:\n  motor:\n    kind: motor\n    command: velocity\n    target:\n      kind: joint\n      id: wheel_joint\n",
        )?;
        std::fs::write(
            root.path().join("components/fixture/structure.urdf"),
            r#"<robot name="component"><link name="base"/><link name="wheel"/><joint name="wheel_joint" type="continuous"><parent link="base"/><child link="wheel"/></joint></robot>"#,
        )?;
        write_lock(root.path(), &[])?;
        Ok(root)
    }

    /// (Re)write `Cargo.lock` for `root`, covering `phoxal`/`robot` plus one
    /// entry per name in `extra_packages`. `resolve_locked_project` runs `cargo
    /// metadata --locked`, which fails if the lock does not already cover every
    /// workspace member, so this must be called after the member crates exist
    /// (and, for a workspace member, after [`declare_workspace_member`]) and
    /// before `resolve()`.
    fn write_lock(root: &std::path::Path, extra_packages: &[&str]) -> anyhow::Result<()> {
        let mut lock = String::from(
            "version = 4\n\n[[package]]\nname = \"fixture\"\nversion = \"0.1.0\"\n\n[[package]]\nname = \"phoxal\"\nversion = \"0.1.0\"\n\n",
        );
        for name in extra_packages {
            lock.push_str(&format!(
                "[[package]]\nname = \"{name}\"\nversion = \"0.1.0\"\n\n"
            ));
        }
        lock.push_str(
            "[[package]]\nname = \"robot\"\nversion = \"0.1.0\"\ndependencies = [\"phoxal\"]\n",
        );
        std::fs::write(root.join("Cargo.lock"), lock)?;
        Ok(())
    }

    /// Turn `root`'s plain root brain package into a real Cargo workspace
    /// listing itself plus `member` (a `services/` or `components/`
    /// crate a test just created). `locked_project_root` deliberately declares no
    /// `[workspace]` table - a glob member errors when a test's temp dir has no
    /// matching crate yet - so a test that adds one calls this with the exact
    /// relative path instead.
    fn declare_workspace_member(root: &std::path::Path, member: &str) -> anyhow::Result<()> {
        std::fs::write(
            root.join("Cargo.toml"),
            format!(
                "[package]\nname = \"robot\"\nversion = \"0.1.0\"\nedition = \"2024\"\npublish = false\n\n[workspace]\nmembers = [\".\", \"components/fixture\", \"{member}\"]\nresolver = \"2\"\n\n[dependencies]\nphoxal = {{ path = \"train/phoxal\" }}\n"
            ),
        )?;
        Ok(())
    }

    fn minimal_robot(extra: &str) -> anyhow::Result<Robot> {
        minimal_robot_with_components("{}", extra)
    }

    fn minimal_robot_with_components(components: &str, extra: &str) -> anyhow::Result<Robot> {
        let (components, actuators) = if components.trim() == "{}" {
            (
                "\n    drive:\n      component: fixture\n      mount_link: base",
                "[drive.motor]",
            )
        } else {
            (components, "[left_drive.motor]")
        };
        crate::source::resolver::parse_robot_from_string(&format!(
            r#"schema: phoxal/robot/v0
robot:
  id: testbot
  motion_limits:
    max_linear_speed_mps: 0.6
    max_angular_speed_radps: 2.0
  kinematic:
    kind: omnidirectional
    actuators: {actuators}
    encoders: []
  components: {components}
{extra}"#
        ))
    }

    /// Persist the real compiler inputs that production resolution consumes.
    ///
    /// These tests used to pass only an already-parsed manifest into a test-only
    /// resolution fork. Keeping the source tree explicit exercises the same
    /// single compiler path as `check`, `run`, `simulate`, and `build`.
    fn write_compiler_sources(root: &std::path::Path, robot: &Robot) -> anyhow::Result<()> {
        crate::source::resolver::write_robot_to_dir(robot, root)?;
        let structure = root.join(&robot.robot.structure);
        if let Some(parent) = structure.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(
            structure,
            r#"<robot name="fixture"><link name="base_footprint"/><link name="base_link"/><link name="base"/><joint name="root" type="fixed"><parent link="base_footprint"/><child link="base_link"/></joint><joint name="base_mount" type="fixed"><parent link="base_link"/><child link="base"/></joint></robot>"#,
        )?;
        for component_type in robot.used_component_types() {
            let component_root = root.join("components").join(component_type);
            std::fs::create_dir_all(&component_root)?;
            std::fs::write(
                component_root.join("component.yaml"),
                "schema: phoxal/component/v0\ncapabilities:\n  motor:\n    kind: motor\n    command: velocity\n    target:\n      kind: joint\n      id: wheel_joint\n",
            )?;
            std::fs::write(
                component_root.join("structure.urdf"),
                r#"<robot name="component"><link name="base"/><link name="wheel"/><joint name="wheel_joint" type="continuous"><parent link="base"/><child link="wheel"/></joint></robot>"#,
            )?;
        }
        Ok(())
    }

    fn resolve_fixture(
        robot: &Robot,
        root: &std::path::Path,
        options: ResolveOptions,
    ) -> anyhow::Result<BundlePlan> {
        write_compiler_sources(root, robot)?;
        resolve(robot, root, options)
    }

    #[derive(Default)]
    struct RecordingReporter(std::sync::Mutex<Vec<crate::PreparationEvent>>);

    impl crate::Reporter for RecordingReporter {
        fn report(&self, event: crate::PreparationEvent) {
            self.0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(event);
        }
    }

    #[test]
    fn container_resolution_compiles_once_and_rejects_profile_drift() -> anyhow::Result<()> {
        let robot = minimal_robot("")?;
        let project = locked_project_root()?;
        write_compiler_sources(project.path(), &robot)?;
        let reporter = RecordingReporter::default();

        let mut resolved = crate::build::resolve_container_staging(
            project.path(),
            &project.path().join(".phoxal/cache/registry"),
            "aarch64-unknown-linux-gnu",
            false,
            &reporter,
        )?;
        assert!(
            resolved
                .set_materialization_build(crate::build::profile::StagingBuild::host_runtime())
                .is_err(),
            "a host-runtime profile must not replace a native-bundle resolution"
        );
        let target_dir = tempfile::tempdir()?;
        resolved.set_materialization_build(
            crate::build::profile::StagingBuild::prebuilt_native_bundle(
                "aarch64-unknown-linux-gnu".to_string(),
                target_dir.path().to_path_buf(),
                None,
            ),
        )?;

        // Later staging consumes `ResolvedStagingInput` directly and has no path
        // back to `resolve_staging`; this count pins the production container
        // helper that creates the sole compiler phase.
        let events = reporter
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let compile_phases = events
            .iter()
            .filter(|event| {
                matches!(
                    event,
                    crate::PreparationEvent::PhaseStarted { id, .. }
                        if id.to_string() == "validate"
                )
            })
            .count();
        assert_eq!(
            compile_phases, 1,
            "container package selection must produce one manifest compilation"
        );
        Ok(())
    }

    #[test]
    fn container_snapshot_uses_the_live_registry_cache_for_components_and_metadata()
    -> anyhow::Result<()> {
        use std::sync::atomic::{AtomicUsize, Ordering};

        use flate2::Compression;
        use flate2::write::GzEncoder;
        use sha2::{Digest, Sha256};

        struct NoHttp(AtomicUsize);
        impl crate::registry_package::RegistryHttp for NoHttp {
            fn get(&self, url: &str) -> anyhow::Result<Vec<u8>> {
                self.0.fetch_add(1, Ordering::SeqCst);
                anyhow::bail!("offline cache unexpectedly requested {url}")
            }
        }

        let robot = minimal_robot_with_components(
            r#"
    left_drive:
      component: wheel
      mount_link: base
"#,
            "",
        )?;
        let snapshot = locked_project_root()?;
        crate::source::resolver::write_robot_to_dir(&robot, snapshot.path())?;
        std::fs::write(
            snapshot.path().join("structure.urdf"),
            r#"<robot name="fixture"><link name="base_footprint"/><link name="base_link"/><link name="base"/><joint name="root" type="fixed"><parent link="base_footprint"/><child link="base_link"/></joint><joint name="base_mount" type="fixed"><parent link="base_link"/><child link="base"/></joint></robot>"#,
        )?;

        let live = tempfile::tempdir()?;
        let cache_root = live.path().join(".phoxal/cache/registry");
        let package = phoxal_cli_catalog::cargo_package_name("phoxal/component-wheel");
        let version = "0.1.0";
        let manifest = format!(
            "[package]\nname = {package:?}\nversion = {version:?}\n\n[[bin]]\nname = {package:?}\npath = \"src/main.rs\"\n"
        );
        let encoder = GzEncoder::new(Vec::new(), Compression::default());
        let mut archive = tar::Builder::new(encoder);
        for (path, bytes) in [
        ("Cargo.toml", manifest.as_bytes()),
        ("src/main.rs", b"fn main() {}" as &[u8]),
        ("component.yaml", b"schema: phoxal/component/v0\ncapabilities:\n  motor:\n    kind: motor\n    command: velocity\n    target:\n      kind: joint\n      id: wheel_joint\n" as &[u8]),
        ("structure.urdf", br#"<robot name="component"><link name="base"/><link name="wheel"/><joint name="wheel_joint" type="continuous"><parent link="base"/><child link="wheel"/></joint></robot>"# as &[u8]),
    ] {
        let mut header = tar::Header::new_gnu();
        header.set_size(bytes.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        archive.append_data(
            &mut header,
            format!("{package}-{version}/{path}"),
            bytes,
        )?;
    }
        let bytes = archive.into_inner()?.finish()?;
        let checksum = hex::encode(Sha256::digest(&bytes));
        let cache_dir = cache_root.join(&package).join(version);
        std::fs::create_dir_all(&cache_dir)?;
        std::fs::write(cache_dir.join(format!("{checksum}.crate")), bytes)?;

        let reporter = RecordingReporter::default();
        let resolved = crate::build::resolve_container_staging(
            snapshot.path(),
            &cache_root,
            "aarch64-unknown-linux-gnu",
            true,
            &reporter,
        )?;
        assert_eq!(resolved.resolved().components.len(), 1);
        assert!(
            resolved.resolved().components[0]
                .assets_root
                .starts_with(&cache_root)
        );
        assert!(!snapshot.path().join(".phoxal/cache/registry").exists());

        let no_http = NoHttp(AtomicUsize::new(0));
        let metadata_cache = crate::registry_package::PackageCache::new(cache_root.clone());
        assert!(
            crate::registry_package::fetch_registry_package(
                &no_http,
                &metadata_cache,
                &package,
                version,
                true,
            )?
            .manifest()?
            .contains(&package)
        );
        assert_eq!(no_http.0.load(Ordering::SeqCst), 0);
        assert_eq!(
            std::fs::read_dir(&cache_dir)?
                .filter_map(Result::ok)
                .filter(|entry| entry
                    .path()
                    .extension()
                    .is_some_and(|extension| extension == "crate"))
                .count(),
            1
        );
        Ok(())
    }

    /// The reserved brain identity is the framework's rule, enforced by the
    /// document grammar itself: a `services.brain` entry never becomes a
    /// `Robot` at all, so no resolution-time guard can be reached by one and
    /// the CLI must not carry a second copy of the rule.
    #[test]
    fn the_reserved_brain_identity_is_rejected_by_the_document_grammar() {
        let error = minimal_robot("services:\n  brain: {}\n")
            .expect_err("the reserved brain identity must not parse");
        assert!(
            format!("{error:#}").contains("reserved for the mandatory root brain"),
            "{error:#}"
        );
    }

    /// An official identity under `services:` is how an operator configures a
    /// service that runs either way. It is not a declaration of anything new,
    /// so it must resolve with no `services/<id>` workspace crate present.
    #[test]
    fn a_declared_official_service_needs_no_workspace_crate() -> anyhow::Result<()> {
        let robot = minimal_robot("services:\n  drive:\n    config: { rate_hz: 50 }\n")?;
        let project = locked_project_root()?;

        write_compiler_sources(project.path(), &robot)?;
        let resolved = resolve_fixture(&robot, project.path(), ResolveOptions::default())?;

        assert!(
            resolved.user_runtimes.is_empty(),
            "a configured official is not a user service: {:?}",
            resolved.user_runtimes
        );
        assert!(
            resolved
                .platform_runtimes
                .iter()
                .any(|runtime| runtime.name == "drive" && runtime.path_override.is_none()),
            "the official still resolves from the catalog: {:?}",
            resolved.platform_runtimes
        );
        Ok(())
    }

    #[test]
    fn platform_runtimes_resolve_from_the_catalog_at_the_locked_train() -> anyhow::Result<()> {
        let robot = minimal_robot("")?;
        let project = locked_project_root()?;

        let resolved = resolve_fixture(&robot, project.path(), ResolveOptions::default())?;

        assert_eq!(resolved.train.version(), "0.1.0");
        let drive = resolved
            .platform_runtimes
            .iter()
            .find(|runtime| runtime.package == "phoxal/service-drive")
            .expect("drive is a catalog service");
        assert_eq!(drive.name, "drive");
        assert_eq!(drive.train, "0.1.0");
        assert_eq!(drive.target.as_deref(), Some(host_target_triple().as_str()));
        assert!(drive.path_override.is_none());

        // Every catalog service is present; the official set is CLI-internal, not
        // subject to any per-robot pruning.
        assert_eq!(
            resolved.platform_runtimes.len(),
            phoxal_cli_catalog::Catalog::official()
                .native()
                .filter(|official| official.kind == ArtifactKind::Service)
                .count()
        );

        // Anything the supervisor absorbed - or that became a local CLI concern -
        // must never resolve as an artifact: a stale catalog entry here is not a
        // compile error, it is a `cargo install` failure at run time for a package
        // the train no longer publishes.
        for absorbed in [
            "router",
            "asset",
            "tool-bus",
            "tool-device",
            "tool-log",
            "tool-telemetry",
            "tool-joypad",
            // The whole `tool-` family is gone; catch any survivor by prefix too.
            "phoxal/tool-",
        ] {
            assert!(
                !resolved
                    .platform_runtimes
                    .iter()
                    .any(|runtime| runtime.package.contains(absorbed)),
                "{absorbed} must not be a resolved artifact"
            );
        }
        Ok(())
    }

    /// The official set a resolution produces is services only. The Webots
    /// controller used to be resolved beside them as a simulator artifact; it
    /// is a host tool on its own train now and never enters a robot graph.
    #[test]
    fn resolution_never_produces_a_simulator_runtime() -> anyhow::Result<()> {
        let robot = minimal_robot("")?;
        let project = locked_project_root()?;

        let resolved = resolve_fixture(&robot, project.path(), ResolveOptions::default())?;
        assert!(
            resolved
                .platform_runtimes
                .iter()
                .all(|runtime| runtime.kind == ArtifactKind::Service),
            "a resolved robot graph is services and component drivers only"
        );
        assert!(
            !resolved
                .platform_runtimes
                .iter()
                .any(|runtime| runtime.package.contains("simulator")),
            "the Webots controller is not a catalog artifact"
        );
        Ok(())
    }

    #[test]
    fn a_matching_workspace_service_crate_overrides_the_official_binary_without_declaration()
    -> anyhow::Result<()> {
        let robot = minimal_robot("")?;
        let project = locked_project_root()?;
        let crate_dir = project.path().join("services/drive");
        std::fs::create_dir_all(crate_dir.join("src"))?;
        std::fs::write(
            crate_dir.join("Cargo.toml"),
            "[package]\nname = \"drive\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[[bin]]\nname = \"drive\"\npath = \"src/main.rs\"\n",
        )?;
        std::fs::write(crate_dir.join("src/main.rs"), "fn main() {}")?;
        declare_workspace_member(project.path(), "services/drive")?;
        write_lock(project.path(), &["drive"])?;
        let crate_dir = crate_dir.canonicalize()?;

        let resolved = resolve_fixture(&robot, project.path(), ResolveOptions::default())?;

        let drive = resolved
            .platform_runtimes
            .iter()
            .find(|runtime| runtime.package == "phoxal/service-drive")
            .expect("drive still resolves as the official identity");
        assert_eq!(drive.path_override.as_deref(), Some(crate_dir.as_path()));
        assert_eq!(resolved.path_overrides.len(), 1);
        assert_eq!(resolved.path_overrides[0].artifact_name, "drive");
        Ok(())
    }

    #[test]
    fn a_declared_user_service_with_no_workspace_crate_fails_resolution() -> anyhow::Result<()> {
        let robot = minimal_robot("services:\n  mission: {}\n")?;
        let project = locked_project_root()?;

        write_compiler_sources(project.path(), &robot)?;
        let error = resolve(&robot, project.path(), ResolveOptions::default())
            .expect_err("a declared service with no matching crate must fail");
        assert!(
            format!("{error:#}").contains("services/ workspace crate"),
            "{error:#}"
        );
        Ok(())
    }

    #[test]
    fn an_undiscovered_workspace_service_is_a_drift_diagnostic_not_an_error() -> anyhow::Result<()>
    {
        let robot = minimal_robot("")?;
        let project = locked_project_root()?;
        let crate_dir = project.path().join("services/mission");
        std::fs::create_dir_all(crate_dir.join("src"))?;
        std::fs::write(
            crate_dir.join("Cargo.toml"),
            "[package]\nname = \"mission\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[[bin]]\nname = \"mission\"\npath = \"src/main.rs\"\n",
        )?;
        std::fs::write(crate_dir.join("src/main.rs"), "fn main() {}")?;
        declare_workspace_member(project.path(), "services/mission")?;
        write_lock(project.path(), &["mission"])?;

        let resolved = resolve_fixture(&robot, project.path(), ResolveOptions::default())?;
        assert_eq!(resolved.undeclared_runtimes.len(), 1);
        assert_eq!(resolved.undeclared_runtimes[0].name, "mission");
        Ok(())
    }

    /// A workspace `components/<id>` crate is resolved without ever touching the
    /// registry (`resolve_components` skips the generated manifest entirely when
    /// every component is workspace-provided).
    #[test]
    fn a_workspace_component_resolves_its_assets_and_driver_without_the_registry()
    -> anyhow::Result<()> {
        let robot = minimal_robot_with_components(
            r#"
    left_drive:
      component: ddsm115
      mount_link: base
      driver:
        connection:
          type: serial
          port: /dev/ttyUSB0
          baud: 115200
"#,
            "",
        )?;
        let project = locked_project_root()?;
        let crate_dir = project.path().join("components/ddsm115");
        std::fs::create_dir_all(crate_dir.join("src"))?;
        std::fs::write(
            crate_dir.join("Cargo.toml"),
            "[package]\nname = \"ddsm115\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[[bin]]\nname = \"ddsm115\"\npath = \"src/main.rs\"\n",
        )?;
        std::fs::write(crate_dir.join("src/main.rs"), "fn main() {}")?;
        std::fs::write(
            crate_dir.join("component.yaml"),
            "schema: phoxal/component/v0\n",
        )?;
        declare_workspace_member(project.path(), "components/ddsm115")?;
        write_lock(project.path(), &["ddsm115"])?;
        let crate_dir = crate_dir.canonicalize()?;

        let resolved = resolve_fixture(&robot, project.path(), ResolveOptions::default())?;
        assert_eq!(resolved.components.len(), 1);
        let component = &resolved.components[0];
        assert_eq!(component.assets_root, crate_dir);
        let driver = component.driver.as_ref().expect("driver resolved");
        assert_eq!(driver.source_path(), Some(crate_dir.as_path()));
        Ok(())
    }

    #[test]
    fn local_component_roots_are_independent_of_driver_intent() -> anyhow::Result<()> {
        let driverless_robot = minimal_robot_with_components(
            r#"
    left_drive:
      component: fixture
      mount_link: base
"#,
            "",
        )?;
        let driver_project = locked_project_root()?;
        let resolved = resolve_fixture(
            &driverless_robot,
            driver_project.path(),
            ResolveOptions::default(),
        )?;
        assert_eq!(resolved.components.len(), 1);
        assert!(resolved.components[0].driver.is_none());
        assert_eq!(
            resolved.components[0].assets_root,
            driver_project
                .path()
                .join("components/fixture")
                .canonicalize()?
        );

        let asset_only_project = locked_project_root()?;
        let fixture = asset_only_project.path().join("components/fixture");
        std::fs::remove_file(fixture.join("Cargo.toml"))?;
        std::fs::remove_dir_all(fixture.join("src"))?;
        std::fs::write(
            asset_only_project.path().join("Cargo.toml"),
            "[package]\nname = \"robot\"\nversion = \"0.1.0\"\nedition = \"2024\"\npublish = false\n\n[workspace]\nmembers = [\".\"]\nresolver = \"2\"\n\n[dependencies]\nphoxal = { path = \"train/phoxal\" }\n",
        )?;
        let declared_driver_robot = minimal_robot_with_components(
            r#"
    left_drive:
      component: fixture
      mount_link: base
      driver:
        connection:
          type: serial
          port: /dev/ttyUSB0
          baud: 115200
"#,
            "",
        )?;
        let error = resolve_fixture(
            &declared_driver_robot,
            asset_only_project.path(),
            ResolveOptions::default(),
        )
        .expect_err("a local asset-only override must not fall through to the registry");
        assert!(error.to_string().contains("asset-only"), "{error:#}");
        Ok(())
    }

    #[test]
    fn direct_component_scan_distinguishes_asset_only_and_invalid_driver_shapes()
    -> anyhow::Result<()> {
        let project = locked_project_root()?;
        let fixture = project.path().join("components/fixture");
        let package = |bins: &[&str], has_library| crate::source::train::WorkspaceComponentCrate {
            manifest_path: fixture.join("Cargo.toml").canonicalize().unwrap(),
            crate_dir: fixture.canonicalize().unwrap(),
            binary_names: bins.iter().map(|name| (*name).to_string()).collect(),
            has_library,
        };
        std::fs::remove_file(fixture.join("Cargo.toml"))?;
        std::fs::remove_dir_all(fixture.join("src"))?;
        let discovered = discover_local_components_from_locked(project.path(), &[])?;
        assert!(discovered["fixture"].driver_crate.is_none());

        std::fs::create_dir_all(fixture.join("src"))?;
        std::fs::write(fixture.join("src/lib.rs"), "")?;
        let error = discover_local_components_from_locked(project.path(), &[])
            .expect_err("assets with source but no Cargo.toml are invalid");
        assert!(error.to_string().contains("asset-only"), "{error:#}");

        std::fs::write(
            fixture.join("Cargo.toml"),
            "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[lib]\npath = \"src/lib.rs\"\n",
        )?;
        let error = discover_local_components_from_locked(project.path(), &[])
            .expect_err("lib-only component driver is invalid");
        assert!(
            error.to_string().contains("obsolete anchor/assets crate"),
            "{error:#}"
        );

        std::fs::write(fixture.join("src/main.rs"), "fn main() {}")?;
        std::fs::create_dir_all(fixture.join("src/bin"))?;
        std::fs::write(fixture.join("src/bin/extra.rs"), "fn main() {}")?;
        let error = discover_local_components_from_locked(
            project.path(),
            &[package(&["fixture", "extra"], false)],
        )
        .expect_err("a driver must have exactly one binary and no library");
        assert!(
            error.to_string().contains("exactly one binary"),
            "{error:#}"
        );

        std::fs::remove_file(fixture.join("src/bin/extra.rs"))?;

        std::fs::write(
            fixture.join("Cargo.toml"),
            "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[lib]\npath = \"src/lib.rs\"\ncrate-type = [\"cdylib\"]\n\n[[bin]]\nname = \"fixture\"\npath = \"src/main.rs\"\n",
        )?;
        let error =
            discover_local_components_from_locked(project.path(), &[package(&["fixture"], true)])
                .expect_err("mixed library and driver targets are invalid");
        assert!(
            error.to_string().contains("must not define a library"),
            "{error:#}"
        );

        std::fs::remove_file(fixture.join("src/lib.rs"))?;
        std::fs::write(
            fixture.join("Cargo.toml"),
            "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[[bin]]\nname = \"fixture\"\npath = \"src/main.rs\"\n",
        )?;
        let missing = project.path().join("components/missing");
        std::fs::create_dir_all(&missing)?;
        let error =
            discover_local_components_from_locked(project.path(), &[package(&["fixture"], false)])
                .expect_err("every direct component needs component.yaml");
        assert!(
            error.to_string().contains("missing component.yaml"),
            "{error:#}"
        );
        std::fs::remove_dir_all(&missing)?;

        let nonmember = project.path().join("components/nonmember");
        std::fs::create_dir_all(nonmember.join("src"))?;
        std::fs::write(
            nonmember.join("component.yaml"),
            "schema: phoxal/component/v0\n",
        )?;
        std::fs::write(
            nonmember.join("Cargo.toml"),
            "[package]\nname = \"nonmember\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[[bin]]\nname = \"nonmember\"\npath = \"src/main.rs\"\n",
        )?;
        std::fs::write(nonmember.join("src/main.rs"), "fn main() {}")?;
        let error =
            discover_local_components_from_locked(project.path(), &[package(&["fixture"], false)])
                .expect_err("a local driver must join a locked workspace");
        assert!(error.to_string().contains("workspace.members"), "{error:#}");
        Ok(())
    }

    #[test]
    fn direct_component_driver_requires_the_root_locked_workspace() -> anyhow::Result<()> {
        let project = locked_project_root()?;
        std::fs::remove_file(project.path().join("Cargo.lock"))?;
        let error = discover_local_components(project.path(), false)
            .expect_err("member driver needs the root lock");
        assert!(
            error.to_string().contains("missing committed Cargo.lock"),
            "{error:#}"
        );

        let standalone = tempfile::tempdir()?;
        let component = standalone.path().join("components/standalone");
        std::fs::create_dir_all(component.join("src"))?;
        std::fs::write(
            component.join("component.yaml"),
            "schema: phoxal/component/v0\n",
        )?;
        std::fs::write(
            component.join("Cargo.toml"),
            "[package]\nname = \"standalone\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[workspace]\n\n[[bin]]\nname = \"standalone\"\npath = \"src/main.rs\"\n",
        )?;
        std::fs::write(component.join("src/main.rs"), "fn main() {}")?;
        let error = discover_local_components(standalone.path(), true)
            .expect_err("standalone drivers are not container-buildable");
        assert!(error.to_string().contains("root Cargo.toml"), "{error:#}");
        std::fs::write(component.join("Cargo.lock"), "version = 4\n")?;
        let error = discover_local_components(standalone.path(), true)
            .expect_err("a standalone lock must not restore standalone driver support");
        assert!(error.to_string().contains("root Cargo.toml"), "{error:#}");
        Ok(())
    }

    #[test]
    fn registry_component_resolution_fetches_distinct_ids_once_and_keeps_excluded_driver_assets()
    -> anyhow::Result<()> {
        use std::collections::{BTreeMap, BTreeSet};
        use std::sync::atomic::{AtomicUsize, Ordering};

        use sha2::Digest;

        struct Http {
            responses: BTreeMap<String, Vec<u8>>,
            downloads: AtomicUsize,
        }
        impl crate::registry_package::RegistryHttp for Http {
            fn get(&self, url: &str) -> anyhow::Result<Vec<u8>> {
                if url.contains("download.invalid") {
                    self.downloads.fetch_add(1, Ordering::SeqCst);
                }
                self.responses
                    .get(url)
                    .cloned()
                    .ok_or_else(|| anyhow::anyhow!("unexpected fake URL {url}"))
            }
        }
        fn archive(package: &str, version: &str) -> anyhow::Result<Vec<u8>> {
            use flate2::Compression;
            use flate2::write::GzEncoder;
            let manifest = format!(
                "[package]\nname = {package:?}\nversion = {version:?}\n\n[[bin]]\nname = {package:?}\npath = \"src/main.rs\"\n"
            );
            let encoder = GzEncoder::new(Vec::new(), Compression::default());
            let mut tar = tar::Builder::new(encoder);
            for (path, bytes) in [
            ("Cargo.toml", manifest.as_bytes()),
            ("src/main.rs", b"fn main() {}" as &[u8]),
            ("component.yaml", b"schema: phoxal/component/v0\ncapabilities:\n  motor:\n    kind: motor\n    command: velocity\n    target:\n      kind: joint\n      id: wheel_joint\n" as &[u8]),
            ("structure.urdf", br#"<robot name="component"><link name="base"/><link name="wheel"/><joint name="wheel_joint" type="continuous"><parent link="base"/><child link="wheel"/></joint></robot>"# as &[u8]),
        ] {
            let mut header = tar::Header::new_gnu();
            header.set_size(bytes.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            tar.append_data(&mut header, format!("{package}-{version}/{path}"), bytes)?;
        }
            Ok(tar.into_inner()?.finish()?)
        }

        let version = "0.1.0";
        let base = "https://phoxal.github.io/registry";
        let mut responses = BTreeMap::from([(
            format!("{base}/config.json"),
            br#"{"dl":"https://download.invalid/{lowerprefix}/{crate}/{version}.crate"}"#.to_vec(),
        )]);
        for id in ["left", "right"] {
            let package = phoxal_cli_catalog::cargo_package_name(&format!("phoxal/component-{id}"));
            let bytes = archive(&package, version)?;
            let checksum = hex::encode(sha2::Sha256::digest(&bytes));
            let index = crate::registry_package::index_path(&package)?;
            responses.insert(
                format!("{base}/{index}"),
                format!(r#"{{"vers":"{version}","cksum":"{checksum}"}}"#).into_bytes(),
            );
            let prefix = index.rsplit_once('/').unwrap().0;
            responses.insert(
                format!("https://download.invalid/{prefix}/{package}/{version}.crate"),
                bytes,
            );
        }
        let http = Http {
            responses,
            downloads: AtomicUsize::new(0),
        };
        let cache_root = tempfile::tempdir()?;
        let cache = crate::registry_package::PackageCache::new(cache_root.path().to_path_buf());
        let ids = BTreeSet::from(["left".to_string(), "right".to_string()]);
        let roots = resolve_registry_component_roots(&ids, &http, &cache, version, false)?;
        assert_eq!(roots.len(), 2);
        assert_eq!(http.downloads.load(Ordering::SeqCst), 2);
        let repeated = resolve_registry_component_roots(&ids, &http, &cache, version, false)?;
        assert_eq!(repeated, roots);
        assert_eq!(http.downloads.load(Ordering::SeqCst), 2);

        let project = locked_project_root()?;
        let project_cache = crate::registry_package::PackageCache::new(
            project.path().join(".phoxal/cache/registry"),
        );
        let one_id = BTreeSet::from(["left".to_string()]);
        resolve_registry_component_roots(&one_id, &http, &project_cache, version, false)?;
        let robot = minimal_robot_with_components(
            r#"
    left_drive:
      component: left
      mount_link: base
      driver:
        connection:
          type: serial
          port: /dev/ttyUSB0
          baud: 115200
"#,
            "",
        )?;
        crate::source::resolver::write_robot_to_dir(&robot, project.path())?;
        std::fs::write(
            project.path().join("structure.urdf"),
            r#"<robot name="fixture"><link name="base_footprint"/><link name="base_link"/><link name="base"/><joint name="root" type="fixed"><parent link="base_footprint"/><child link="base_link"/></joint><joint name="base_mount" type="fixed"><parent link="base_link"/><child link="base"/></joint></robot>"#,
        )?;
        let resolved = resolve(
            &robot,
            project.path(),
            ResolveOptions {
                offline: true,
                ..Default::default()
            },
        )?;
        let component = &resolved.components[0];
        assert!(component.assets_root.join("component.yaml").is_file());
        assert!(
            component.driver.is_some(),
            "a declared driver is always resolved: one bundle serves every mode"
        );
        Ok(())
    }

    /// A declared driver always resolves. `--drivers off` is a launch
    /// decision the CLI applies when it starts runtimes, so it cannot reach
    /// back into resolution and change what the bundle contains.
    #[test]
    fn a_declared_driver_always_resolves_its_package() -> anyhow::Result<()> {
        let robot = minimal_robot_with_components(
            r#"
    left_drive:
      component: ddsm115
      mount_link: base
      driver:
        connection:
          type: serial
          port: /dev/ttyUSB0
          baud: 115200
"#,
            "",
        )?;
        let project = locked_project_root()?;
        let crate_dir = project.path().join("components/ddsm115");
        std::fs::create_dir_all(crate_dir.join("src"))?;
        std::fs::write(
            crate_dir.join("Cargo.toml"),
            "[package]\nname = \"ddsm115\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[[bin]]\nname = \"ddsm115\"\npath = \"src/main.rs\"\n",
        )?;
        std::fs::write(crate_dir.join("src/main.rs"), "fn main() {}")?;
        std::fs::write(
            crate_dir.join("component.yaml"),
            "schema: phoxal/component/v0\n",
        )?;
        declare_workspace_member(project.path(), "components/ddsm115")?;
        write_lock(project.path(), &["ddsm115"])?;

        let resolved = resolve_fixture(&robot, project.path(), ResolveOptions::default())?;
        let component = &resolved.components[0];
        assert!(
            resolved.source_manifest.robot.components["left_drive"]
                .driver
                .is_some(),
            "declared intent is preserved"
        );
        assert!(
            matches!(
                component.driver,
                Some(crate::source::resolver::ResolvedComponentDriver::Local { .. })
            ),
            "the workspace driver crate is resolved for every mode"
        );
        Ok(())
    }

    #[test]
    fn resolve_target_triple_accepts_aliases_and_full_triples() {
        assert_eq!(
            resolve_target_triple("aarch64").unwrap(),
            "aarch64-unknown-linux-gnu"
        );
        assert_eq!(
            resolve_target_triple("arm64").unwrap(),
            "aarch64-unknown-linux-gnu"
        );
        assert_eq!(
            resolve_target_triple("x86_64").unwrap(),
            "x86_64-unknown-linux-gnu"
        );
        assert_eq!(
            resolve_target_triple("riscv64gc-unknown-linux-gnu").unwrap(),
            "riscv64gc-unknown-linux-gnu"
        );
        assert!(resolve_target_triple("nonsense").is_err());
    }
}
