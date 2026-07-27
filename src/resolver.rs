use std::collections::BTreeSet;
use std::path::Path;

use anyhow::{Context, Result, anyhow};
use phoxal::model::robot::RobotV0 as Robot;
use phoxal_cli_core::project::catalog::{self, ArtifactKind};
pub use phoxal_cli_core::project::host_target_triple;
use phoxal_cli_core::project::resolve_manifest::{
    ComponentDependency, resolve_manifest_package_dirs, write_resolve_manifest,
};
use phoxal_cli_core::project::tooling::hash_tree;

/// The provider every official Phoxal package uses in catalog identities.
const PHOXAL_PROVIDER: &str = "phoxal";

use phoxal_cli_core::project::resolver::{
    ResolveOptions, ResolvedComponent, ResolvedComponentPackage, ResolvedComponentSource,
    ResolvedPathOverride, ResolvedPathOverrideKind, ResolvedPlatformRuntime, ResolvedRobot,
    ResolvedTool, ResolvedUserRuntime, UndeclaredRuntime, official_binary_name,
};

/// Resolve a robot manifest against the CLI-internal official catalog
/// (organization#951 WS4): no suite fetch, no vendored artifact store. Every
/// official service, tool, the infrastructure router, and every component
/// driver package materializes later via `cargo install` at exactly the
/// locked framework train; official component *assets* resolve their
/// on-disk directory via the generated `.phoxal/resolve/Cargo.toml` and
/// `cargo metadata` here, since staging needs to read `component.yaml` and
/// friends without a binary to install.
pub fn resolve(
    robot: &Robot,
    project_root: &Path,
    options: ResolveOptions,
) -> Result<ResolvedRobot> {
    // Declaration invariants are the very first check (#950): an invalid
    // workspace lock must not mask a dual/official declaration error.
    phoxal_cli_core::project::layout::validate_runtime_declarations(robot)?;
    let project = phoxal_cli_core::project::train::resolve_locked_project(project_root)?;
    let train = project.train.version.clone();
    let target = options
        .official_target_triple
        .clone()
        .unwrap_or_else(host_target_triple);
    // Finding A3: robot.yaml structural/schema validation always genuinely
    // runs (never conditionally skipped like materialization), so it always
    // gets its own truthful "validate" phase rather than a synthetic single
    // "Preparing" phase.
    crate::session::diagnostics::run_phase(
        phoxal_cli_core::session::event::PhaseId::new("validate"),
        "Validating robot.yaml".to_string(),
        || {
            robot
                .validate()
                .map_err(|errors| anyhow!("Robot errors:\n{}", join_errors(errors)))
        },
    )?;
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

    let mut components = resolve_components(
        robot,
        project_root,
        &train,
        &target,
        &project.runtimes,
        &options.drivers,
    )?;

    let mut tools = catalog::NATIVE
        .iter()
        .filter(|official| official.kind == ArtifactKind::Tool)
        .map(|official| tool_from_official(official, &train, &tool_target))
        .collect::<Vec<_>>();
    tools.extend(
        catalog::NATIVE
            .iter()
            .filter(|official| official.kind == ArtifactKind::Infrastructure)
            .map(|official| tool_from_official(official, &train, &tool_target)),
    );

    let workspace = apply_workspace_runtimes(
        robot,
        project_root,
        &project.runtimes,
        &mut platform_runtimes,
        &mut simulators,
        &mut components,
        &mut tools,
        &options.drivers,
    )?;

    Ok(ResolvedRobot {
        robot: robot.clone(),
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
        ArtifactKind::Infrastructure => "phoxal/infrastructure-",
        ArtifactKind::ComponentAssets | ArtifactKind::ComponentDriver => "phoxal/component-",
    };
    package.strip_prefix(prefix).unwrap_or(package).to_string()
}

/// Resolve every `robot.components.<instance>` entry. A component whose id
/// matches a `components/` workspace crate resolves from that crate; every
/// other component resolves its assets directory from the registry via the
/// generated `.phoxal/resolve/Cargo.toml` (one `cargo metadata` call for
/// every distinct registry component this robot uses), and - when the
/// instance declares a driver and the policy keeps it - a driver package
/// that materializes later through the identical `cargo install` path a
/// service does.
fn resolve_components(
    robot: &Robot,
    project_root: &Path,
    train: &str,
    target: &str,
    workspace_runtimes: &[phoxal_cli_core::project::train::WorkspaceRuntime],
    drivers: &phoxal_cli_core::project::layout::DriverSelection,
) -> Result<Vec<ResolvedComponent>> {
    let workspace_component = |component_id: &str| {
        workspace_runtimes.iter().find(|runtime| {
            runtime.kind == phoxal_cli_core::project::train::WorkspaceRuntimeKind::Component
                && runtime.crate_dir.file_name().and_then(|name| name.to_str())
                    == Some(component_id)
        })
    };

    // Batch every distinct registry-sourced component into ONE generated
    // manifest and ONE `cargo metadata` resolution, rather than one per
    // instance/component.
    let mut registry_component_ids = BTreeSet::new();
    for instance in robot.robot.components.values() {
        if workspace_component(&instance.component).is_none() {
            registry_component_ids.insert(instance.component.clone());
        }
    }
    let resolved_dirs = if registry_component_ids.is_empty() {
        std::collections::BTreeMap::new()
    } else {
        let dependencies = registry_component_ids
            .iter()
            .map(|component_id| ComponentDependency {
                catalog_id: format!("{PHOXAL_PROVIDER}/component-{component_id}"),
                train: train.to_string(),
            })
            .collect::<Vec<_>>();
        let manifest_path = write_resolve_manifest(project_root, &dependencies)
            .context("failed to write the generated component-resolution manifest")?;
        resolve_manifest_package_dirs(&manifest_path)
            .context("failed to resolve official component packages via `cargo metadata`")?
    };

    let mut components = Vec::new();
    for (instance_name, instance) in &robot.robot.components {
        let component_id = &instance.component;
        let package = format!("{PHOXAL_PROVIDER}/component-{component_id}");
        let has_driver = instance.driver.is_some();

        if let Some(runtime) = workspace_component(component_id) {
            let assets_dir = runtime
                .component_assets
                .as_ref()
                .context("component workspace runtime has no component assets")?;
            anyhow::ensure!(
                runtime.binary_names.is_empty() != has_driver,
                "components/{component_id} bin target presence must match robot component instance {instance_name} driver presence"
            );
            components.push(ResolvedComponent {
                instance: instance_name.clone(),
                source_name: component_id.clone(),
                assets: ResolvedComponentPackage {
                    package: format!("workspace/component-{component_id}"),
                    kind: ArtifactKind::ComponentAssets,
                    source: ResolvedComponentSource::Path {
                        path: assets_dir.clone(),
                    },
                    resolved_dir: Some(assets_dir.clone()),
                    registry_runtime: None,
                },
                driver: (has_driver && drivers.includes_instance(instance_name)).then(|| {
                    ResolvedComponentPackage {
                        package: format!("workspace/component-{component_id}"),
                        kind: ArtifactKind::ComponentDriver,
                        source: ResolvedComponentSource::Path {
                            path: runtime.crate_dir.clone(),
                        },
                        resolved_dir: Some(runtime.crate_dir.clone()),
                        registry_runtime: None,
                    }
                }),
                has_driver,
            });
            continue;
        }

        let cargo_name = phoxal_cli_core::project::catalog::cargo_package_name(&package);
        let resolved_dir = resolved_dirs.get(&cargo_name).cloned().with_context(|| {
            format!(
                "robot.components.{instance_name}.component '{component_id}' failed to resolve its \
                 component_assets package {package} from the registry"
            )
        })?;
        let assets = ResolvedComponentPackage {
            package: package.clone(),
            kind: ArtifactKind::ComponentAssets,
            source: ResolvedComponentSource::Registry,
            resolved_dir: Some(resolved_dir),
            registry_runtime: None,
        };
        let driver = (has_driver && drivers.includes_instance(instance_name)).then(|| {
            ResolvedComponentPackage {
                package: package.clone(),
                kind: ArtifactKind::ComponentDriver,
                source: ResolvedComponentSource::Registry,
                resolved_dir: None,
                registry_runtime: Some(ResolvedPlatformRuntime {
                    name: component_id.clone(),
                    package: package.clone(),
                    kind: ArtifactKind::ComponentDriver,
                    path_override: None,
                    train: train.to_string(),
                    target: Some(target.to_string()),
                }),
            }
        });

        components.push(ResolvedComponent {
            instance: instance_name.clone(),
            source_name: component_id.clone(),
            assets,
            driver,
            has_driver,
        });
    }
    Ok(components)
}

#[allow(clippy::too_many_arguments)]
fn apply_workspace_runtimes(
    robot: &Robot,
    project_root: &Path,
    runtimes: &[phoxal_cli_core::project::train::WorkspaceRuntime],
    platform_runtimes: &mut [ResolvedPlatformRuntime],
    _simulators: &mut [ResolvedPlatformRuntime],
    components: &mut [ResolvedComponent],
    tools: &mut [ResolvedTool],
    drivers: &phoxal_cli_core::project::layout::DriverSelection,
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
            WorkspaceRuntimeKind::Component => {
                let matching = components
                    .iter_mut()
                    .filter(|component| component.source_name == logical_name)
                    .collect::<Vec<_>>();
                if matching.is_empty() {
                    continue;
                }
                for component in matching {
                    let assets_dir = runtime
                        .component_assets
                        .as_ref()
                        .context("component workspace runtime has no component assets")?;
                    component.assets = ResolvedComponentPackage {
                        package: format!("workspace/component-{logical_name}"),
                        kind: ArtifactKind::ComponentAssets,
                        source: ResolvedComponentSource::Path {
                            path: assets_dir.clone(),
                        },
                        resolved_dir: Some(assets_dir.clone()),
                        registry_runtime: None,
                    };
                    if runtime.binary_names.is_empty() {
                        anyhow::ensure!(
                            !component.has_driver,
                            "robot component instance {} declares a driver, but components/{logical_name} is lib-only",
                            component.instance
                        );
                        component.driver = None;
                    } else {
                        anyhow::ensure!(
                            component.has_driver,
                            "components/{logical_name} has a bin target, but robot component instance {} has no driver connection",
                            component.instance
                        );
                        // The driver policy gates resolution here too (#936):
                        // an excluded workspace driver keeps `driver: None`, so
                        // it never enters the source participants or the source
                        // check and its crate is never built.
                        component.driver =
                            drivers.includes_instance(&component.instance).then(|| {
                                ResolvedComponentPackage {
                                    package: format!("workspace/component-{logical_name}"),
                                    kind: ArtifactKind::ComponentDriver,
                                    source: ResolvedComponentSource::Path {
                                        path: runtime.crate_dir.clone(),
                                    },
                                    resolved_dir: Some(runtime.crate_dir.clone()),
                                    registry_runtime: None,
                                }
                            });
                    }
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

fn join_errors(errors: Vec<phoxal::model::robot::ValidationError>) -> String {
    errors
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests;
