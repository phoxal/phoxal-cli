use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use anyhow::{Context, Result, anyhow};
use phoxal_cli_core::project::catalog::{self, ArtifactKind};
pub use phoxal_cli_core::project::host_target_triple;
use phoxal_cli_core::project::tooling::hash_tree;
use phoxal_manifest::source::robot::v0::Manifest as Robot;

/// The provider every official Phoxal package uses in catalog identities.
const PHOXAL_PROVIDER: &str = "phoxal";

use phoxal_cli_core::project::resolver::{
    BundlePlan, CompiledBundle, ResolveOptions, ResolvedComponent, ResolvedComponentDriver,
    ResolvedPathOverride, ResolvedPathOverrideKind, ResolvedPlatformRuntime, ResolvedTool,
    ResolvedUserRuntime, UndeclaredRuntime, official_binary_name,
};

/// Resolve a robot manifest against the CLI-internal official catalog
/// (organization#951 WS4): no suite fetch, no vendored artifact store. Every
/// official service, tool, and every component
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
    // Declaration invariants are the very first check (#950): an invalid
    // workspace lock must not mask a dual/official declaration error.
    phoxal_cli_core::project::layout::validate_runtime_declarations(robot)?;
    let project =
        phoxal_cli_core::project::train::resolve_locked_project(project_root, options.offline)?;
    resolved_train(&project.train.version);
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
    project: &phoxal_cli_core::project::train::LockedProject,
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
    project: &phoxal_cli_core::project::train::LockedProject,
) -> Result<BundlePlan> {
    // The catalog below is one current snapshot, not per-train history
    // (organization#951 WS4 review, medium 3): reject a locked train it
    // predates before applying it, rather than silently resolving an
    // official set that never existed for that train.
    catalog::ensure_train_supported(&project.train)?;
    let train = project.train.version.clone();
    let target = options
        .official_target_triple
        .clone()
        .unwrap_or_else(host_target_triple);
    robot
        .validate()
        .map_err(|errors| anyhow!("Robot errors:\n{}", join_errors(errors)))?;
    let tool_target = options
        .tool_target_triple
        .unwrap_or_else(host_target_triple);

    let mut platform_runtimes = catalog::NATIVE
        .iter()
        .filter(|official| official.kind == ArtifactKind::Service)
        .map(|official| platform_runtime_from_official(official, &train, Some(&target)))
        .collect::<Vec<_>>();
    // Simulator artifacts execute HOST-side under Webots and never belong to
    // an installed native robot bundle. Host run/check/simulation resolution
    // keeps them; `phoxal build` explicitly omits them so a physical target
    // build does not require a Webots controller for that target.
    let mut simulators = if options.include_simulators {
        catalog::WEBOTS
            .iter()
            .map(|official| {
                platform_runtime_from_official(official, &train, Some(&host_target_triple()))
            })
            .collect()
    } else {
        Vec::new()
    };

    let components = resolve_components(
        robot,
        ComponentResolutionContext {
            project_root,
            registry_cache_root,
            train: &train,
            target: &target,
            drivers: &options.drivers,
            local_packages: &project.local_components,
            offline: options.offline,
        },
    )?;

    let mut tools = catalog::NATIVE
        .iter()
        .filter(|official| official.kind == ArtifactKind::Tool)
        .map(|official| tool_from_official(official, &train, &tool_target))
        .collect::<Vec<_>>();

    let workspace = apply_workspace_runtimes(
        robot,
        project_root,
        &project.runtimes,
        &mut platform_runtimes,
        &mut simulators,
        &mut tools,
    )?;

    let component_roots = components
        .iter()
        .map(|component| (component.source_name.clone(), component.assets_root.clone()))
        .collect::<BTreeMap<_, _>>();
    let robot_manifest = project_root.join("robot.yaml");
    let compiled = CompiledBundle::from_project(
        phoxal_manifest::compile(phoxal_manifest::SourceSet {
            project_root: project_root.to_path_buf(),
            robot_manifest,
            component_roots,
        })
        .context("failed to compile the resolved source project")?,
    )?;

    Ok(BundlePlan {
        source_manifest: robot.clone(),
        compiled,
        train,
        target,
        platform_runtimes,
        simulators,
        user_runtimes: workspace.user_runtimes,
        user_tools: workspace.user_tools,
        undeclared_runtimes: workspace.undeclared_runtimes,
        components,
        tools,
        path_overrides: workspace.path_overrides,
    })
}

fn platform_runtime_from_official(
    official: &catalog::OfficialRuntime,
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

fn tool_from_official(
    official: &catalog::OfficialRuntime,
    train: &str,
    target: &str,
) -> ResolvedTool {
    let short = short_name(official.package, official.kind);
    ResolvedTool {
        kind: official.kind,
        name: format!("{}-{short}", official.kind.wire_kind()),
        package: official.package.to_string(),
        binary_name: official_binary_name(official.kind, &short),
        path_override: None,
        train: train.to_string(),
        target: target.to_string(),
    }
}

fn short_name(package: &str, kind: ArtifactKind) -> String {
    let prefix = match kind {
        ArtifactKind::Service => "phoxal/service-",
        ArtifactKind::Tool => "phoxal/tool-",
        ArtifactKind::Simulator => "phoxal/simulator-",
        ArtifactKind::ComponentDriver => "phoxal/component-",
    };
    package.strip_prefix(prefix).unwrap_or(package).to_string()
}

/// Resolve authored component roots directly from `components/<id>/` or the
/// exact locked registry package. Cargo workspace membership is deliberately
/// irrelevant to definition discovery.
struct ComponentResolutionContext<'a> {
    project_root: &'a Path,
    registry_cache_root: &'a Path,
    train: &'a str,
    target: &'a str,
    drivers: &'a phoxal_cli_core::project::layout::DriverSelection,
    local_packages: &'a [phoxal_cli_core::project::train::WorkspaceComponentCrate],
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
            let driver = match (
                &local.driver_crate,
                declares_driver,
                context.drivers.includes_instance(instance_name),
            ) {
                (Some(crate_dir), true, true) => Some(ResolvedComponentDriver::Local {
                    crate_dir: crate_dir.clone(),
                }),
                (None, true, _) => anyhow::bail!(
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

        let assets_root = registry_roots
            .get(component_id)
            .expect("registry roots were resolved from every non-local component id")
            .clone();
        let driver =
            (declares_driver && context.drivers.includes_instance(instance_name)).then(|| {
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
            let package = phoxal_cli_core::project::catalog::cargo_package_name(&format!(
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
    local_packages: &[phoxal_cli_core::project::train::WorkspaceComponentCrate],
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
    let root_metadata = fs::symlink_metadata(&root)
        .with_context(|| format!("failed to inspect {}", root.display()))?;
    anyhow::ensure!(
        !root_metadata.file_type().is_symlink(),
        "components directory {} must not be a symlink; use a direct authored directory under the project root",
        root.display()
    );
    let mut components = BTreeMap::new();
    for entry in
        fs::read_dir(&root).with_context(|| format!("failed to read {}", root.display()))?
    {
        let entry = entry.with_context(|| format!("failed to read {} entry", root.display()))?;
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)
            .with_context(|| format!("failed to inspect local component {}", path.display()))?;
        if metadata.file_type().is_symlink() && path.is_dir() {
            anyhow::bail!(
                "local component directory {} must not be a symlink; use a direct authored directory under components/",
                path.display()
            );
        }
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
    package: &phoxal_cli_core::project::train::WorkspaceComponentCrate,
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
    runtimes: &[phoxal_cli_core::project::train::WorkspaceRuntime],
    platform_runtimes: &mut [ResolvedPlatformRuntime],
    _simulators: &mut [ResolvedPlatformRuntime],
    tools: &mut [ResolvedTool],
) -> Result<WorkspaceRuntimeResolution> {
    use phoxal_cli_core::project::train::WorkspaceRuntimeKind;

    let mut user_runtimes = Vec::new();
    let mut user_tools = Vec::new();
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
        match runtime.kind {
            WorkspaceRuntimeKind::Service => {
                let official_package = format!("phoxal/service-{logical_name}");
                if let Some(official) = platform_runtimes
                    .iter_mut()
                    .find(|entry| entry.package == official_package)
                {
                    official.path_override = Some(runtime.crate_dir.clone());
                    overrides.push(ResolvedPathOverride {
                        key: official_package,
                        kind: ResolvedPathOverrideKind::Service,
                        artifact_name: logical_name,
                        path: runtime.crate_dir.clone(),
                    });
                } else if robot.services.contains_key(&logical_name) {
                    // Declared: the services map selects which discovered
                    // workspace services belong to the robot (#950).
                    user_runtimes.push(ResolvedUserRuntime {
                        name: logical_name,
                        path: relative,
                        source_hash: hash_tree(&runtime.crate_dir)?,
                    });
                } else {
                    // Present but undeclared: legal, not built or launched;
                    // surfaced as a drift diagnostic (#950).
                    undeclared.push(UndeclaredRuntime {
                        name: logical_name,
                        family: "services",
                    });
                }
            }
            WorkspaceRuntimeKind::Tool => {
                let official_package = format!("phoxal/tool-{logical_name}");
                // A `tools/` workspace crate whose name matches an official
                // tool identity overrides that official binary - a source
                // override, never a declaration. A non-official crate is a
                // user tool: the `tools:` map in robot.yaml selects it (#950);
                // present-but-undeclared is a drift diagnostic, not an error.
                if let Some(official) = tools
                    .iter_mut()
                    .find(|entry| entry.package == official_package)
                {
                    official.path_override = Some(runtime.crate_dir.clone());
                    overrides.push(ResolvedPathOverride {
                        key: official_package,
                        kind: ResolvedPathOverrideKind::Tool,
                        artifact_name: logical_name,
                        path: runtime.crate_dir.clone(),
                    });
                } else if robot.tools.contains_key(&logical_name) {
                    user_tools.push(ResolvedUserRuntime {
                        name: logical_name,
                        path: relative,
                        source_hash: hash_tree(&runtime.crate_dir)?,
                    });
                } else {
                    undeclared.push(UndeclaredRuntime {
                        name: logical_name,
                        family: "tools",
                    });
                }
            }
        }
    }
    let discovered_services = user_runtimes
        .iter()
        .map(|runtime| runtime.name.as_str())
        .chain(
            overrides
                .iter()
                .filter(|override_| override_.kind == ResolvedPathOverrideKind::Service)
                .map(|override_| override_.artifact_name.as_str()),
        )
        .collect::<std::collections::BTreeSet<_>>();
    let missing = robot
        .services
        .keys()
        .filter(|name| !discovered_services.contains(name.as_str()))
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
    // The tools declaration validates both ways too (#950): a declared tool
    // needs its workspace crate, may not name an official identity (officials
    // are catalog-owned and configless), and may not collide with a declared
    // service (both would claim bin/<name>).
    let discovered_tools = user_tools
        .iter()
        .map(|tool| tool.name.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    for name in robot.tools.keys() {
        anyhow::ensure!(
            !tools
                .iter()
                .any(|entry| entry.package == format!("phoxal/tool-{name}")),
            "robot.yaml declares tools.{name}, but '{name}' is an official tool; official tools \
             are catalog-owned, always run, and take no robot.yaml declaration (a tools/{name} \
             workspace crate overrides the official binary without being declared)"
        );
        anyhow::ensure!(
            discovered_tools.contains(name.as_str()),
            "robot.yaml declares tools.{name} with no matching tools/ workspace crate"
        );
        anyhow::ensure!(
            !robot.services.contains_key(name.as_str()),
            "robot.yaml declares '{name}' under both services and tools; the two maps share one \
             binary namespace, so a name may appear in only one"
        );
    }
    overrides.sort_by(|left, right| left.key.cmp(&right.key));
    user_tools.sort_by(|left, right| left.name.cmp(&right.name));
    undeclared.sort_by(|left, right| (left.family, &left.name).cmp(&(right.family, &right.name)));
    Ok(WorkspaceRuntimeResolution {
        user_runtimes,
        user_tools,
        undeclared_runtimes: undeclared,
        path_overrides: overrides,
    })
}

#[cfg(test)]
fn discover_local_components(
    project_root: &Path,
    offline: bool,
) -> Result<BTreeMap<String, LocalComponent>> {
    let project = phoxal_cli_core::project::train::resolve_locked_project(project_root, offline)?;
    discover_local_components_from_locked(project_root, &project.local_components)
}

/// The workspace-runtime half of resolution (#950): the declared user services
/// and tools, the drift records for undeclared crates, and the official-source
/// overrides applied along the way.
#[derive(Debug)]
struct WorkspaceRuntimeResolution {
    user_runtimes: Vec<ResolvedUserRuntime>,
    user_tools: Vec<ResolvedUserRuntime>,
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

fn join_errors(errors: Vec<phoxal_manifest::source::robot::v0::ValidationError>) -> String {
    errors
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
