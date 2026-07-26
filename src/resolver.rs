use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use phoxal::model::robot::RobotV0 as Robot;
pub use phoxal_cli_core::project::suite::host_target_triple;
use phoxal_cli_core::project::suite::{
    ArtifactKind, Kind, Suite, artifacts_of_kind, select_artifact,
};
use phoxal_cli_core::project::tooling::hash_tree;

/// The provider every official Phoxal package uses in suite identities.
const PHOXAL_PROVIDER: &str = "phoxal";

use phoxal_cli_core::project::resolver::{
    ComponentDriverUnavailable, ResolveOptions, ResolvedComponent, ResolvedComponentPackage,
    ResolvedComponentSource, ResolvedPathOverride, ResolvedPathOverrideKind,
    ResolvedPlatformRuntime, ResolvedRobot, ResolvedTool, ResolvedUserRuntime, UndeclaredRuntime,
    official_binary_name,
};

pub fn resolve(
    robot: &Robot,
    project_root: &Path,
    suite: Option<&Suite>,
    options: ResolveOptions,
) -> Result<ResolvedRobot> {
    // Declaration invariants are the very first check (#950): an invalid
    // workspace lock or an absent suite must not mask a dual/official
    // declaration error.
    phoxal_cli_core::project::layout::validate_runtime_declarations(robot)?;
    let suite = suite.context(
        "the locked framework train suite is required for resolution; restore network access or pass --suite <path> to the immutable suite.json",
    )?;
    let train = suite.version.clone();
    let project = phoxal_cli_core::project::train::resolve_locked_project(project_root)?;
    anyhow::ensure!(
        project.train.version == train,
        "Cargo metadata selected framework train {}, but suite inventory is {}",
        project.train.version,
        train
    );
    let target = options
        .official_target_triple
        .clone()
        .unwrap_or_else(host_target_triple);
    // Finding A3: robot.yaml structural/schema validation always genuinely
    // runs (never conditionally skipped like download/build), so it always
    // gets its own truthful "validate" phase rather than the old synthetic
    // single "Preparing" phase.
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
    let prefer_vendored = crate::host_paths::artifacts_dir().is_ok_and(|path| path.is_dir());
    let _artifact_lock = prefer_vendored
        .then(crate::native_artifacts::ArtifactStoreLock::shared)
        .transpose()?;
    let mut platform_runtimes = resolve_suite_entries(
        robot,
        suite,
        Kind::Service,
        ArtifactKind::Service,
        &target,
        prefer_vendored,
    )?;
    // Simulator artifacts execute HOST-side under Webots and never belong to
    // an installed native robot bundle. Host run/check/simulation resolution
    // keeps them; `phoxal build` explicitly omits them so a physical aarch64
    // build does not require a nonexistent aarch64 Webots-controller artifact.
    let mut simulators = if options.include_simulators {
        resolve_suite_entries(
            robot,
            suite,
            Kind::Simulator,
            ArtifactKind::Simulator,
            &host_target_triple(),
            prefer_vendored,
        )?
    } else {
        Vec::new()
    };

    let mut components = resolve_components(&ComponentResolveContext {
        robot,
        suite: Some(suite),
        train: &train,
        target: &target,
        workspace_runtimes: &project.runtimes,
        prefer_vendored,
        drivers: &options.drivers,
    })?;
    let mut tools = resolve_tools(robot, Some(suite), &train, &tool_target, prefer_vendored)?;
    tools.extend(resolve_native_artifacts(
        robot,
        Some(suite),
        &train,
        &tool_target,
        prefer_vendored,
        Kind::Infrastructure,
        ArtifactKind::Infrastructure,
    )?);
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
                    apply_platform_runtime_path_override(official, &runtime.crate_dir);
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
                    official.asset = format!("path:{}", runtime.crate_dir.display());
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
                        path_override: Some(assets_dir.clone()),
                        suite_runtime: None,
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
                                    path_override: Some(runtime.crate_dir.clone()),
                                    suite_runtime: None,
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

/// Replace an official suite runtime with a Cargo workspace source package.
fn apply_platform_runtime_path_override(runtime: &mut ResolvedPlatformRuntime, path: &Path) {
    runtime.path_override = Some(path.to_path_buf());
    runtime.artifact_ref = format!("path:{}", path.display());
    runtime.sha256 = None;
    runtime.url = None;
    runtime.size = None;
    runtime.published = true;
    runtime.published_triples = Vec::new();
}

fn resolve_suite_entries(
    robot: &Robot,
    suite: &Suite,
    suite_kind: Kind,
    kind: ArtifactKind,
    target: &str,
    prefer_vendored: bool,
) -> Result<Vec<ResolvedPlatformRuntime>> {
    artifacts_of_kind(suite, suite_kind)
        .into_iter()
        .map(|artifact| {
            let name = short_name(&artifact.id, suite_kind);
            resolved_runtime_from_expected_package(
                robot,
                suite,
                ExpectedArtifact {
                    kind,
                    name: &name,
                    package: &artifact.id,
                    train: &suite.version,
                    target: Some(target),
                    assets: false,
                    prefer_vendored,
                },
            )
        })
        .collect()
}

fn vendored_runtime(
    name: &str,
    package: &str,
    kind: ArtifactKind,
    train: &str,
    target: Option<&str>,
) -> Result<ResolvedPlatformRuntime> {
    let version = crate::native_artifacts::active_version_for(package)?.with_context(|| {
        format!(
            "suite unreachable and vendored package {package} has no active version; run `phoxal update` online"
        )
    })?;
    anyhow::ensure!(
        version == train,
        "vendored package {package} belongs to framework train {version}, but Cargo.lock selects {train}; run `phoxal update` online"
    );
    let scope = match target {
        Some(target) => crate::native_artifacts::artifact_target_dir_for(package, target)?,
        None => crate::native_artifacts::artifact_assets_dir_for(package)?,
    };
    anyhow::ensure!(
        scope.is_dir(),
        "suite unreachable and vendored package {package} active version {version} has no {}; run `phoxal update` online",
        target.map_or("assets", |target| target)
    );
    Ok(ResolvedPlatformRuntime {
        name: name.to_string(),
        package: package.to_string(),
        kind,
        version: version.clone(),
        artifact_ref: format!(
            "vendored:{package}@{version}:{}",
            target.unwrap_or("assets")
        ),
        sha256: None,
        url: None,
        size: None,
        published: true,
        published_triples: target.into_iter().map(str::to_string).collect(),
        path_override: None,
        train: train.to_string(),
        target: target.map(str::to_string),
    })
}

struct ExpectedArtifact<'a> {
    kind: ArtifactKind,
    name: &'a str,
    package: &'a str,
    train: &'a str,
    target: Option<&'a str>,
    assets: bool,
    prefer_vendored: bool,
}

fn resolved_runtime_from_expected_package(
    _robot: &Robot,
    suite: &Suite,
    expected: ExpectedArtifact<'_>,
) -> Result<ResolvedPlatformRuntime> {
    let ExpectedArtifact {
        kind,
        name,
        package,
        train,
        target,
        assets,
        prefer_vendored,
    } = expected;
    if prefer_vendored && let Ok(runtime) = vendored_runtime(name, package, kind, train, target) {
        return Ok(runtime);
    }
    let entry = if assets {
        suite
            .artifacts
            .iter()
            .find(|artifact| artifact.id == package)
            .with_context(|| {
                format!(
                    "required artifact {package} is absent from train {} suite",
                    suite.version
                )
            })?
    } else {
        select_artifact(
            suite,
            package,
            target.context("target artifact is missing a target triple")?,
        )?
    };
    let built = if assets {
        entry.assets.as_ref()
    } else {
        entry
            .targets
            .get(target.context("target artifact is missing a target triple")?)
    };
    let artifact_ref = built.map_or_else(
        || {
            format!(
                "{}:{}-{}",
                filesystem_safe_package_name(package),
                suite.version,
                target.unwrap_or("assets")
            )
        },
        |blob| blob.url.clone(),
    );
    Ok(ResolvedPlatformRuntime {
        name: name.to_string(),
        package: package.to_string(),
        kind,
        version: suite.version.clone(),
        artifact_ref,
        sha256: built.map(|blob| blob.sha256.clone()),
        url: built.map(|blob| blob.url.clone()),
        size: built.map(|blob| blob.size),
        published: built.is_some(),
        published_triples: entry.targets.keys().cloned().collect(),
        path_override: None,
        train: train.to_string(),
        target: target.map(str::to_string),
    })
}

/// Resolve a user-supplied `--target` selector to the full suite target
/// triple official artifacts are published under. Accepts the short arch aliases
/// (`aarch64`/`arm64`, `x86_64`/`amd64`) or a full triple passed through as-is.
/// Official artifacts publish gnu Linux assets, so a bare arch maps to the gnu
/// triple.
pub fn resolve_target_triple(selector: &str) -> Result<String> {
    Ok(match selector {
        "aarch64" | "arm64" => "aarch64-unknown-linux-gnu".to_string(),
        "x86_64" | "amd64" => "x86_64-unknown-linux-gnu".to_string(),
        other if other.contains('-') => other.to_string(),
        other => bail!(
            "unrecognized --target '{other}'; expected aarch64, x86_64, or a full target triple"
        ),
    })
}

/// Resolve every `robot.components.<instance>` entry from the flattened
/// `phoxal/component-<id>` artifact. Its assets are used for every instance;
/// its target blob is also used when the instance declares a `driver` block.
///
/// Shared resolution context for [`resolve_component_package`]: the pieces
/// every component package slot (assets or driver) needs, bundled so the
/// per-slot resolver stays under clippy's argument-count lint.
struct ComponentResolveContext<'a> {
    robot: &'a Robot,
    suite: Option<&'a Suite>,
    train: &'a str,
    target: &'a str,
    workspace_runtimes: &'a [phoxal_cli_core::project::train::WorkspaceRuntime],
    prefer_vendored: bool,
    /// Driver instances resolution may resolve binaries for (#936): an
    /// excluded instance keeps its declared `has_driver` but gets no resolved
    /// driver slot, so nothing downstream selects, fetches, builds, or checks
    /// its driver binary.
    drivers: &'a phoxal_cli_core::project::layout::DriverSelection,
}

fn resolve_components(context: &ComponentResolveContext<'_>) -> Result<Vec<ResolvedComponent>> {
    let robot = context.robot;
    let mut components = Vec::new();
    for (instance_name, instance) in &robot.robot.components {
        let component_id = &instance.component;
        let package = format!("{PHOXAL_PROVIDER}/component-{component_id}");

        let has_driver = instance.driver.is_some();
        if let Some(runtime) = context.workspace_runtimes.iter().find(|runtime| {
            runtime.kind == phoxal_cli_core::project::train::WorkspaceRuntimeKind::Component
                && runtime.crate_dir.file_name().and_then(|name| name.to_str())
                    == Some(component_id)
        }) {
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
                    path_override: Some(assets_dir.clone()),
                    suite_runtime: None,
                },
                driver: (has_driver && context.drivers.includes_instance(instance_name)).then(
                    || ResolvedComponentPackage {
                        package: format!("workspace/component-{component_id}"),
                        kind: ArtifactKind::ComponentDriver,
                        source: ResolvedComponentSource::Path {
                            path: runtime.crate_dir.clone(),
                        },
                        path_override: Some(runtime.crate_dir.clone()),
                        suite_runtime: None,
                    },
                ),
                has_driver,
            });
            continue;
        }
        // Every component outside the workspace must resolve its assets
        // package - driverless included. The old driverless `Err -> None`
        // swallow had no real producer (the one real driverless component,
        // a driverless component is a workspace crate) and hid unknown
        // packages, unsupported targets, and malformed suite state behind a
        // silent "assetless" (#936). If genuinely assetless suite components
        // become real, the suite/catalog models that explicitly.
        let assets = resolve_component_package(context, &package, ArtifactKind::ComponentAssets)
            .map_err(|err| {
                err.context(format!(
                    "robot.components.{instance_name}.component '{component_id}' failed to resolve its component_assets package"
                ))
            })?;

        let driver = if has_driver && context.drivers.includes_instance(instance_name) {
            match resolve_component_package(context, &package, ArtifactKind::ComponentDriver) {
                Ok(driver) => Some(driver),
                Err(_) => {
                    return Err(ComponentDriverUnavailable {
                        instance: instance_name.clone(),
                        component: component_id.clone(),
                    }
                    .into());
                }
            }
        } else {
            None
        };

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

/// Resolve one component package slot (`component_assets` or
/// `component_driver`) for `package` from the suite. A suite resolution also captures the matched entry's
/// built artifact for the needed scope (assets or the
/// resolved target triple for drivers) into `suite_runtime`, exactly like a
/// service/simulator captures `artifact_ref`/`sha256`/`published` - see
/// [`resolved_runtime_from_expected_package`]. If the entry exists but has no
/// built artifact for that scope yet (a metadata-only entry, or not yet
/// published for this target), resolution still succeeds (the entry is real
/// and versioned - a bare `check` on an older version must not hard-fail
/// here), but `suite_runtime` carries `sha256: None, published: false` so a
/// later staging attempt reports a clear diagnostic instead of silently
/// succeeding with no bundle to fetch.
fn resolve_component_package(
    context: &ComponentResolveContext<'_>,
    package: &str,
    kind: ArtifactKind,
) -> Result<ResolvedComponentPackage> {
    let (target, assets) = if kind == ArtifactKind::ComponentAssets {
        (None, true)
    } else {
        (Some(context.target), false)
    };
    let component_name = package.strip_prefix("phoxal/component-").unwrap_or(package);
    let suite_runtime = match context.suite {
        Some(suite) => resolved_runtime_from_expected_package(
            context.robot,
            suite,
            ExpectedArtifact {
                kind,
                name: component_name,
                package,
                train: context.train,
                target,
                assets,
                prefer_vendored: context.prefer_vendored,
            },
        )
        .with_context(|| format!("failed to resolve suite entry for {package}"))?,
        None => vendored_runtime(component_name, package, kind, context.train, target)?,
    };

    Ok(ResolvedComponentPackage {
        package: package.to_string(),
        kind,
        source: ResolvedComponentSource::Suite,
        path_override: None,
        suite_runtime: Some(suite_runtime),
    })
}

fn resolve_tools(
    robot: &Robot,
    suite: Option<&Suite>,
    train: &str,
    target: &str,
    prefer_vendored: bool,
) -> Result<Vec<ResolvedTool>> {
    resolve_native_artifacts(
        robot,
        suite,
        train,
        target,
        prefer_vendored,
        Kind::Tool,
        ArtifactKind::Tool,
    )
}

#[allow(clippy::too_many_arguments)]
fn resolve_native_artifacts(
    _robot: &Robot,
    suite: Option<&Suite>,
    train: &str,
    target: &str,
    prefer_vendored: bool,
    suite_kind: Kind,
    kind: ArtifactKind,
) -> Result<Vec<ResolvedTool>> {
    let Some(suite) = suite else {
        bail!("the locked framework train suite is required to enumerate {kind} artifacts");
    };
    artifacts_of_kind(suite, suite_kind)
        .into_iter()
        .map(|entry| {
            let package = entry.id.as_str();
            let artifact_name = short_name(package, suite_kind);
            if prefer_vendored
                && let Ok(runtime) =
                    vendored_runtime(&artifact_name, package, kind, train, Some(target))
            {
                return Ok(ResolvedTool {
                    kind,
                    name: format!("{}-{artifact_name}", kind.wire_kind()),
                    package: (*package).to_string(),
                    requested: runtime.version.clone(),
                    resolved: runtime.version,
                    repo: "vendored".to_string(),
                    asset: runtime.artifact_ref,
                    binary_name: official_binary_name(kind, &artifact_name),
                    sha256: String::new(),
                    url: None,
                    size: None,
                    published: true,
                    path_override: None,
                    train: train.to_string(),
                    target: target.to_string(),
                });
            }
            let entry = select_artifact(suite, package, target)?;
            let built = entry.targets.get(target);
            let asset = built.map_or_else(
                || format!("{}:{}-{target}", entry.id, suite.version),
                |blob| blob.url.clone(),
            );
            Ok(ResolvedTool {
                kind,
                name: format!("{}-{artifact_name}", kind.wire_kind()),
                package: entry.id.clone(),
                requested: suite.version.clone(),
                resolved: suite.version.clone(),
                repo: "phoxal/framework".to_string(),
                asset,
                binary_name: official_binary_name(kind, &artifact_name),
                sha256: built
                    .map(|blob| blob.sha256.clone())
                    .unwrap_or_else(|| "0".repeat(64)),
                url: built.map(|blob| blob.url.clone()),
                size: built.map(|blob| blob.size),
                published: built.is_some(),
                path_override: None,
                train: train.to_string(),
                target: target.to_string(),
            })
        })
        .collect()
}

/// The filesystem/tag-safe projection of a provider-qualified package id
/// (`phoxal/service-drive` -> `phoxal-service-drive`), used for the synthetic
/// `artifact_ref` fallback when a suite entry has no built artifact yet for
/// the resolved target (the suite's package/target projection).
fn filesystem_safe_package_name(package: &str) -> String {
    package.replace('/', "-")
}

fn short_name(id: &str, kind: Kind) -> String {
    let prefix = match kind {
        Kind::Service => "phoxal/service-",
        Kind::Component => "phoxal/component-",
        Kind::Tool => "phoxal/tool-",
        Kind::Simulator => "phoxal/simulator-",
        Kind::Infrastructure => "phoxal/infrastructure-",
    };
    id.strip_prefix(prefix).unwrap_or(id).to_string()
}

fn join_errors(errors: Vec<phoxal::model::robot::ValidationError>) -> String {
    errors
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host_paths::test_support::ScratchPhoxalHome;
    use phoxal_cli_core::project::resolver::load_robot;
    use phoxal_cli_core::project::suite::{
        fixture_component_assets_entry_for_tests, fixture_service_entry_for_tests,
        fixture_simulator_entry_for_tests, fixture_suite_for_tests,
    };
    use std::path::PathBuf;

    #[test]
    fn an_invalid_declaration_fails_before_the_suite_check() -> anyhow::Result<()> {
        // The declaration validator is the first operation in `resolve` (#950):
        // an official identity in a map must fail with the declaration error
        // even when the suite is absent (which would otherwise be the first
        // failure) - proving the ordering, not just the presence, of the check.
        let _phoxal_home = ScratchPhoxalHome::new()?;
        let robot = Robot::parse_from_string(
            r#"schema: robot/v0
robot:
  id: bot
  namespace: dev
  motion_limits:
    max_linear_speed_mps: 0.6
    max_angular_speed_radps: 2.0
  kinematic:
    kind: omnidirectional
    actuators: []
    encoders: []
  components: {}
tools:
  drive: {}
"#,
        )?;
        let error = resolve(
            &robot,
            std::path::Path::new("."),
            None,
            ResolveOptions {
                ..ResolveOptions::default()
            },
        )
        .expect_err("an official identity in tools: must fail resolution");
        let message = format!("{error:#}");
        assert!(
            message.contains("official service"),
            "the declaration error must win over the absent-suite error: {message}"
        );
        Ok(())
    }

    fn test_suite() -> Suite {
        fixture_suite_for_tests(vec![
            fixture_service_entry_for_tests(
                "drive",
                "0.1.0",
                &host_target_triple(),
                // Published so the package resolves for this host target
                // without robot.yaml needing any pin at all (D1: no
                // `artifacts.generation` ceiling to auto-detect anymore).
                true,
            ),
            fixture_component_assets_entry_for_tests("ddsm115", "0.1.0"),
        ])
    }

    fn locked_project_root() -> anyhow::Result<tempfile::TempDir> {
        let root = tempfile::tempdir()?;
        std::fs::create_dir_all(root.path().join("src"))?;
        std::fs::create_dir_all(root.path().join("train/phoxal/src"))?;
        std::fs::write(
            root.path().join("Cargo.toml"),
            "[package]\nname = \"robot\"\nversion = \"0.1.0\"\nedition = \"2024\"\npublish = false\n\n[dependencies]\nphoxal = { path = \"train/phoxal\" }\n",
        )?;
        std::fs::write(root.path().join("src/lib.rs"), "")?;
        std::fs::write(
            root.path().join("train/phoxal/Cargo.toml"),
            "[package]\nname = \"phoxal\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        )?;
        std::fs::write(root.path().join("train/phoxal/src/lib.rs"), "")?;
        std::fs::write(
            root.path().join("Cargo.lock"),
            "version = 4\n\n[[package]]\nname = \"phoxal\"\nversion = \"0.1.0\"\n\n[[package]]\nname = \"robot\"\nversion = \"0.1.0\"\ndependencies = [\"phoxal\"]\n",
        )?;
        Ok(root)
    }

    #[test]
    fn native_bundle_resolution_never_requires_a_simulator_target() -> anyhow::Result<()> {
        let _phoxal_home = ScratchPhoxalHome::new()?;
        let target = host_target_triple();
        let suite = fixture_suite_for_tests(vec![
            fixture_service_entry_for_tests("drive", "0.1.0", &target, true),
            // The catalog entry exists, but has no binary for this physical
            // target. Host resolution must notice that; Native build
            // resolution must omit the simulator before artifact selection.
            fixture_simulator_entry_for_tests("webots-controller", "0.1.0", &target, false),
            fixture_component_assets_entry_for_tests("ddsm115", "0.1.0"),
        ]);
        let robot = Robot::parse_from_string(
            r#"schema: robot/v0
robot:
  id: testbot
  namespace: test
  motion_limits:
    max_linear_speed_mps: 0.6
    max_angular_speed_radps: 2.0
  kinematic:
    kind: differential
    left_actuators: [left_drive.motor]
    right_actuators: [right_drive.motor]
    left_encoders: [left_drive.encoder]
    right_encoders: [right_drive.encoder]
    wheel_radius_m: 0.1
    wheel_base_m: 0.5
  components:
    left_drive:
      component: ddsm115
      mount_link: left_wheel_mount
    right_drive:
      component: ddsm115
      mount_link: right_wheel_mount
"#,
        )?;
        let project = locked_project_root()?;

        resolve(
            &robot,
            project.path(),
            Some(&suite),
            ResolveOptions::default(),
        )
        .expect_err("host resolution still requires its simulator artifact");

        let resolved = resolve(
            &robot,
            project.path(),
            Some(&suite),
            ResolveOptions {
                include_simulators: false,
                ..ResolveOptions::default()
            },
        )?;
        assert!(
            resolved.simulators.is_empty(),
            "a Native bundle must carry no simulator-only runtimes"
        );
        Ok(())
    }

    #[test]
    fn stale_vendored_train_falls_through_to_the_locked_suite() -> anyhow::Result<()> {
        let _phoxal_home = ScratchPhoxalHome::new()?;
        let package = "phoxal/service-drive";
        let target = host_target_triple();
        let stale = crate::native_artifacts::artifact_package_dir(package)?
            .join("versions")
            .join("0.0.9")
            .join("targets")
            .join(&target);
        std::fs::create_dir_all(stale)?;
        crate::native_artifacts::retarget_active_version(package, "0.0.9")?;

        let robot = Robot::parse_from_string(
            r#"schema: robot/v0
robot:
  id: testbot
  namespace: test
  motion_limits:
    max_linear_speed_mps: 0.6
    max_angular_speed_radps: 2.0
  kinematic:
    kind: differential
    left_actuators: [left_drive.motor]
    right_actuators: [right_drive.motor]
    left_encoders: [left_drive.encoder]
    right_encoders: [right_drive.encoder]
    wheel_radius_m: 0.1
    wheel_base_m: 0.5
  components:
    left_drive:
      component: ddsm115
      mount_link: left_wheel_mount
    right_drive:
      component: ddsm115
      mount_link: right_wheel_mount
"#,
        )?;
        let suite = test_suite();
        let project = locked_project_root()?;
        let resolved = resolve(
            &robot,
            project.path(),
            Some(&suite),
            ResolveOptions::default(),
        )?;

        let drive = resolved
            .platform_runtimes
            .iter()
            .find(|runtime| runtime.package == package)
            .expect("drive resolves from the locked train suite");
        assert_eq!(drive.version, suite.version);
        assert_eq!(drive.train, suite.version);
        assert!(drive.url.is_some(), "stale vendored state must not win");
        Ok(())
    }

    #[test]
    fn an_excluded_driver_is_not_resolved_even_when_its_artifact_is_missing() -> anyhow::Result<()>
    {
        // Round-2 finding 1 (#936): the driver policy gates RESOLUTION, not
        // just staging. `test_suite()` carries ddsm115 assets but NO driver
        // artifact, so resolving the driver would fail - with the driver
        // excluded (`--drivers off`), resolution must succeed and leave the
        // driver slot empty while `has_driver` keeps the declared intent.
        let _phoxal_home = ScratchPhoxalHome::new()?;
        let robot = Robot::parse_from_string(
            r#"schema: robot/v0
robot:
  id: testbot
  namespace: test
  motion_limits:
    max_linear_speed_mps: 0.6
    max_angular_speed_radps: 2.0
  kinematic:
    kind: differential
    left_actuators: [left_drive.motor]
    right_actuators: [right_drive.motor]
    left_encoders: [left_drive.encoder]
    right_encoders: [right_drive.encoder]
    wheel_radius_m: 0.1
    wheel_base_m: 0.5
  components:
    left_drive:
      component: ddsm115
      mount_link: left_wheel_mount
      driver:
        connection: { type: can, bus: 0, node_id: 1 }
"#,
        )?;
        let suite = test_suite();
        let project = locked_project_root()?;

        // Sanity: with the driver selected, the missing driver artifact fails.
        resolve(
            &robot,
            project.path(),
            Some(&suite),
            ResolveOptions {
                ..ResolveOptions::default()
            },
        )
        .expect_err("a selected driver with no suite artifact must fail resolution");

        // Excluded: resolution succeeds and never touches the driver artifact.
        let resolved = resolve(
            &robot,
            project.path(),
            Some(&suite),
            ResolveOptions {
                drivers: phoxal_cli_core::project::layout::DriverSelection::None,
                ..ResolveOptions::default()
            },
        )?;
        let left = resolved
            .components
            .iter()
            .find(|component| component.instance == "left_drive")
            .expect("component resolved");
        assert!(left.has_driver, "the declared driver intent is kept");
        assert!(left.driver.is_none(), "the excluded driver is not resolved");
        Ok(())
    }

    #[test]
    fn driverless_component_absent_from_workspace_and_suite_fails_precisely() -> anyhow::Result<()>
    {
        // A driverless component can be a workspace assets crate and resolves through
        // the workspace branch. A driverless component with NO workspace crate
        // and NO suite package has no real producer - it is a typo or a
        // missing crate - so resolution fails naming the package instead of
        // silently treating the component as assetless (#936: the old
        // `Err -> None` swallow also hid unsupported targets and malformed
        // suite state).
        let _phoxal_home = ScratchPhoxalHome::new()?;
        let robot = Robot::parse_from_string(
            r#"schema: robot/v0
robot:
  id: testbot
  namespace: test
  motion_limits:
    max_linear_speed_mps: 0.6
    max_angular_speed_radps: 2.0
  kinematic:
    kind: differential
    left_actuators: [left_drive.motor]
    right_actuators: [right_drive.motor]
    left_encoders: [left_drive.encoder]
    right_encoders: [right_drive.encoder]
    wheel_radius_m: 0.1
    wheel_base_m: 0.5
  components:
    front_caster:
      component: passive_caster
      mount_link: front_caster_mount
"#,
        )?;
        // `test_suite()` carries no `phoxal/component-passive_caster` entry,
        // and the fixture workspace has no `components/passive_caster` crate.
        let suite = test_suite();
        let project = locked_project_root()?;
        let error = resolve(
            &robot,
            project.path(),
            Some(&suite),
            ResolveOptions {
                ..ResolveOptions::default()
            },
        )
        .expect_err("a component with no workspace crate and no suite package must fail");

        let message = format!("{error:#}");
        assert!(
            message.contains("front_caster") && message.contains("component_assets"),
            "the error must name the instance and the missing assets package, got: {message}"
        );
        Ok(())
    }

    #[test]
    fn driven_component_with_no_official_assets_package_still_hard_fails() -> anyhow::Result<()> {
        // The opposite case: a component that DOES declare a `driver:`
        // block still needs its assets package - a missing assets package
        // for a driven (active) component remains a hard resolution error,
        // not a silent `None`.
        let _phoxal_home = ScratchPhoxalHome::new()?;
        let robot = Robot::parse_from_string(
            r#"schema: robot/v0
robot:
  id: testbot
  namespace: test
  motion_limits:
    max_linear_speed_mps: 0.6
    max_angular_speed_radps: 2.0
  kinematic:
    kind: differential
    left_actuators: [left_drive.motor]
    right_actuators: [right_drive.motor]
    left_encoders: [left_drive.encoder]
    right_encoders: [right_drive.encoder]
    wheel_radius_m: 0.1
    wheel_base_m: 0.5
  components:
    left_drive:
      component: unknown_driven_component
      mount_link: left_wheel_mount
      driver:
        connection: { type: can, bus: 0, node_id: 1 }
"#,
        )?;
        let suite = test_suite();
        let project = locked_project_root()?;
        let error = resolve(
            &robot,
            project.path(),
            Some(&suite),
            ResolveOptions {
                ..ResolveOptions::default()
            },
        )
        .expect_err("a driven component with no suite entry at all must hard-fail");

        let message = format!("{error:#}");
        assert!(
            message.contains(
                "robot.components.left_drive.component 'unknown_driven_component' failed to \
                 resolve its component_assets package"
            ),
            "{message}"
        );
        assert!(
            message.contains(
                "required artifact phoxal/component-unknown_driven_component is absent from train 0.1.0 suite"
            ),
            "{message}"
        );
        Ok(())
    }

    #[test]
    fn load_robot_tolerates_user_service_config() -> anyhow::Result<()> {
        // Service configuration is part of the typed robot model.
        let temp = tempfile::tempdir()?;
        let path = temp.path().join("robot.yaml");
        std::fs::write(
            &path,
            r#"schema: robot/v0
robot:
  id: bot
  namespace: dev
  motion_limits:
    max_linear_speed_mps: 0.6
    max_angular_speed_radps: 2.0
  kinematic:
    kind: differential
    left_actuators: [l.motor]
    right_actuators: [r.motor]
    left_encoders: [l.encoder]
    right_encoders: [r.encoder]
    wheel_radius_m: 0.1
    wheel_base_m: 0.5
services:
  brain:
    config:
      gain: 0.5
"#,
        )?;

        let robot = load_robot(&path)?;
        assert!(robot.services.contains_key("brain"));
        assert_eq!(
            robot
                .services
                .get("brain")
                .and_then(|service| service.config.as_ref()),
            Some(&serde_json::json!({ "gain": 0.5 }))
        );

        Ok(())
    }

    #[test]
    fn load_robot_keeps_router_config() -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        let path = temp.path().join("robot.yaml");
        std::fs::write(
            &path,
            r#"schema: robot/v0
robot:
  id: bot
  namespace: dev
  motion_limits:
    max_linear_speed_mps: 0.6
    max_angular_speed_radps: 2.0
  kinematic:
    kind: differential
    left_actuators: [l.motor]
    right_actuators: [r.motor]
    left_encoders: [l.encoder]
    right_encoders: [r.encoder]
    wheel_radius_m: 0.1
    wheel_base_m: 0.5
router:
  config: config/router.json5
"#,
        )?;

        let loaded = load_robot(&path)?;
        assert_eq!(
            loaded.router.config,
            Some(PathBuf::from("config/router.json5"))
        );
        Ok(())
    }

    #[test]
    fn load_robot_rejects_invalid_launch_ids_and_collisions() -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        let path = temp.path().join("robot.yaml");
        std::fs::write(
            &path,
            r#"schema: robot/v0
robot:
  id: bot
  namespace: dev
  motion_limits:
    max_linear_speed_mps: 0.6
    max_angular_speed_radps: 2.0
  kinematic:
    kind: differential
    left_actuators: [left_drive.motor]
    right_actuators: [right_drive.encoder]
    left_encoders: [left_drive.encoder]
    right_encoders: [right_drive.encoder]
    wheel_radius_m: 0.1
    wheel_base_m: 0.5
  components:
    left_drive:
      component: ddsm115
      mount_link: left
services:
  mission-service: {}
  left_drive: {}
"#,
        )?;

        let error = load_robot(&path).expect_err("launch ids should be checked");
        let message = error.to_string();
        assert!(message.contains("mission-service"), "{message}");
        assert!(message.contains("collides"), "{message}");
        Ok(())
    }

    #[test]
    fn platform_runtime_exposes_native_artifact_ref() {
        let runtime = ResolvedPlatformRuntime {
            name: "asset".to_string(),
            package: "phoxal/service-asset".to_string(),
            kind: ArtifactKind::Service,
            version: "0.1.0".to_string(),
            artifact_ref: "service-asset:v1-stable".to_string(),
            sha256: None,
            url: None,
            size: None,
            published: false,
            published_triples: Vec::new(),
            path_override: None,
            train: "0.36.0".to_string(),
            target: Some("aarch64-unknown-linux-gnu".to_string()),
        };

        assert_eq!(runtime.artifact_ref(), "service-asset:v1-stable");
    }

    #[test]
    fn tools_declaration_selects_and_undeclared_crates_are_drift() -> anyhow::Result<()> {
        use phoxal_cli_core::project::train::{WorkspaceRuntime, WorkspaceRuntimeKind};

        // The `tools:` map selects which non-official tools/ crates belong to
        // the robot (#950): a declared crate becomes a user tool; an
        // undeclared one is legal drift, recorded but not built.
        let robot = Robot::parse_from_string(
            r#"schema: robot/v0
robot:
  id: bot
  namespace: dev
  motion_limits:
    max_linear_speed_mps: 0.6
    max_angular_speed_radps: 2.0
  kinematic:
    kind: omnidirectional
    actuators: []
    encoders: []
  components: {}
tools:
  declared-tool: {}
"#,
        )?;
        let project = tempfile::tempdir()?;
        let runtimes = vec![
            WorkspaceRuntime {
                package: "declared-tool".to_string(),
                crate_dir: project.path().join("tools/declared-tool"),
                kind: WorkspaceRuntimeKind::Tool,
                binary_names: vec!["declared-tool".to_string()],
                component_assets: None,
            },
            WorkspaceRuntime {
                package: "custom".to_string(),
                crate_dir: project.path().join("tools/custom"),
                kind: WorkspaceRuntimeKind::Tool,
                binary_names: vec!["custom".to_string()],
                component_assets: None,
            },
        ];
        std::fs::create_dir_all(project.path().join("tools/declared-tool"))?;
        let mut platform_runtimes = Vec::new();
        let mut simulators = Vec::new();
        let mut components = Vec::new();
        let mut tools = Vec::new();
        let resolution = apply_workspace_runtimes(
            &robot,
            project.path(),
            &runtimes,
            &mut platform_runtimes,
            &mut simulators,
            &mut components,
            &mut tools,
            &phoxal_cli_core::project::layout::DriverSelection::All,
        )?;
        assert_eq!(resolution.user_tools.len(), 1);
        assert_eq!(resolution.user_tools[0].name, "declared-tool");
        assert_eq!(resolution.undeclared_runtimes.len(), 1);
        assert_eq!(resolution.undeclared_runtimes[0].name, "custom");
        assert_eq!(resolution.undeclared_runtimes[0].family, "tools");
        Ok(())
    }

    #[test]
    fn an_excluded_workspace_driver_is_not_reintroduced_after_resolution() -> anyhow::Result<()> {
        use phoxal_cli_core::project::layout::DriverSelection;
        use phoxal_cli_core::project::train::{WorkspaceRuntime, WorkspaceRuntimeKind};

        // Round-3 finding 1 (#936): resolve_components leaves an excluded
        // driver slot empty, but apply_workspace_runtimes used to re-set it
        // unconditionally for a workspace driver crate - reintroducing the
        // excluded driver into the source participants and the source check.
        let robot = Robot::parse_from_string(
            r#"schema: robot/v0
robot:
  id: bot
  namespace: dev
  motion_limits:
    max_linear_speed_mps: 0.6
    max_angular_speed_radps: 2.0
  kinematic:
    kind: differential
    left_actuators: [left_drive.motor]
    right_actuators: [right_drive.motor]
    left_encoders: [left_drive.encoder]
    right_encoders: [right_drive.encoder]
    wheel_radius_m: 0.1
    wheel_base_m: 0.5
  components:
    left_drive:
      component: ddsm115
      mount_link: left_wheel_mount
      driver:
        connection: { type: can, bus: 0, node_id: 1 }
"#,
        )?;
        let project = tempfile::tempdir()?;
        let crate_dir = project.path().join("components/ddsm115");
        std::fs::create_dir_all(&crate_dir)?;
        let make_components = || {
            vec![ResolvedComponent {
                instance: "left_drive".to_string(),
                source_name: "ddsm115".to_string(),
                assets: ResolvedComponentPackage {
                    package: "workspace/component-ddsm115".to_string(),
                    kind: ArtifactKind::ComponentAssets,
                    source: ResolvedComponentSource::Path {
                        path: crate_dir.clone(),
                    },
                    path_override: Some(crate_dir.clone()),
                    suite_runtime: None,
                },
                // resolve_components already left the excluded slot empty.
                driver: None,
                has_driver: true,
            }]
        };
        let runtimes = vec![WorkspaceRuntime {
            package: "ddsm115".to_string(),
            crate_dir: crate_dir.clone(),
            kind: WorkspaceRuntimeKind::Component,
            binary_names: vec!["ddsm115".to_string()],
            component_assets: Some(crate_dir.clone()),
        }];

        // Excluded: the workspace pass must NOT reintroduce the driver.
        let mut components = make_components();
        apply_workspace_runtimes(
            &robot,
            project.path(),
            &runtimes,
            &mut [],
            &mut [],
            &mut components,
            &mut [],
            &DriverSelection::None,
        )?;
        assert!(
            components[0].driver.is_none(),
            "an excluded workspace driver must stay unresolved"
        );

        // Included: the same pass resolves it from the workspace crate.
        let mut components = make_components();
        apply_workspace_runtimes(
            &robot,
            project.path(),
            &runtimes,
            &mut [],
            &mut [],
            &mut components,
            &mut [],
            &DriverSelection::All,
        )?;
        assert!(
            components[0].driver.is_some(),
            "a selected workspace driver resolves from its crate"
        );
        Ok(())
    }

    #[test]
    fn undeclared_service_crates_are_drift_not_members() -> anyhow::Result<()> {
        use phoxal_cli_core::project::train::{WorkspaceRuntime, WorkspaceRuntimeKind};

        // The services map selects (#950): a services/ crate the robot.yaml
        // does not declare is legal drift - never a user runtime.
        let robot = Robot::parse_from_string(
            r#"schema: robot/v0
robot:
  id: bot
  namespace: dev
  motion_limits:
    max_linear_speed_mps: 0.6
    max_angular_speed_radps: 2.0
  kinematic:
    kind: omnidirectional
    actuators: []
    encoders: []
  components: {}
services:
  mission: {}
"#,
        )?;
        let project = tempfile::tempdir()?;
        std::fs::create_dir_all(project.path().join("services/mission"))?;
        let runtimes = vec![
            WorkspaceRuntime {
                package: "mission".to_string(),
                crate_dir: project.path().join("services/mission"),
                kind: WorkspaceRuntimeKind::Service,
                binary_names: vec!["mission".to_string()],
                component_assets: None,
            },
            WorkspaceRuntime {
                package: "experiment".to_string(),
                crate_dir: project.path().join("services/experiment"),
                kind: WorkspaceRuntimeKind::Service,
                binary_names: vec!["experiment".to_string()],
                component_assets: None,
            },
        ];
        let resolution = apply_workspace_runtimes(
            &robot,
            project.path(),
            &runtimes,
            &mut [],
            &mut [],
            &mut [],
            &mut [],
            &phoxal_cli_core::project::layout::DriverSelection::All,
        )?;
        assert_eq!(resolution.user_runtimes.len(), 1);
        assert_eq!(resolution.user_runtimes[0].name, "mission");
        assert_eq!(resolution.undeclared_runtimes.len(), 1);
        assert_eq!(resolution.undeclared_runtimes[0].name, "experiment");
        assert_eq!(resolution.undeclared_runtimes[0].family, "services");
        Ok(())
    }

    #[test]
    fn tool_declaration_rules_fail_precisely() -> anyhow::Result<()> {
        use phoxal_cli_core::project::train::{WorkspaceRuntime, WorkspaceRuntimeKind};

        let project = tempfile::tempdir()?;
        let base = r#"schema: robot/v0
robot:
  id: bot
  namespace: dev
  motion_limits:
    max_linear_speed_mps: 0.6
    max_angular_speed_radps: 2.0
  kinematic:
    kind: omnidirectional
    actuators: []
    encoders: []
  components: {}
"#;
        let run = |yaml: &str, runtimes: &[WorkspaceRuntime], tools: &mut Vec<ResolvedTool>| {
            let robot = Robot::parse_from_string(yaml)?;
            apply_workspace_runtimes(
                &robot,
                project.path(),
                runtimes,
                &mut [],
                &mut [],
                &mut [],
                tools,
                &phoxal_cli_core::project::layout::DriverSelection::All,
            )
        };

        // A declared tool with no matching workspace crate fails.
        let error = run(
            &format!("{base}tools:\n  ghost: {{}}\n"),
            &[],
            &mut Vec::new(),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("tools.ghost"), "{error}");
        assert!(
            error.contains("no matching tools/ workspace crate"),
            "{error}"
        );

        // A declared tool naming an official identity fails: officials are
        // catalog-owned and never declared.
        let mut official_tools = vec![ResolvedTool {
            kind: ArtifactKind::Tool,
            name: "tool-log".to_string(),
            package: "phoxal/tool-log".to_string(),
            requested: "suite".to_string(),
            resolved: "suite".to_string(),
            repo: "suite".to_string(),
            asset: "suite:log".to_string(),
            binary_name: "phoxal-tool-log".to_string(),
            sha256: String::new(),
            url: None,
            size: None,
            published: true,
            path_override: None,
            train: "0.38.1".to_string(),
            target: host_target_triple(),
        }];
        let error = run(
            &format!("{base}tools:\n  log: {{}}\n"),
            &[],
            &mut official_tools,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("official tool"), "{error}");

        // A name declared under both services and tools fails: one binary
        // namespace.
        std::fs::create_dir_all(project.path().join("services/dual"))?;
        std::fs::create_dir_all(project.path().join("tools/dual"))?;
        let runtimes = vec![
            WorkspaceRuntime {
                package: "dual".to_string(),
                crate_dir: project.path().join("services/dual"),
                kind: WorkspaceRuntimeKind::Service,
                binary_names: vec!["dual".to_string()],
                component_assets: None,
            },
            WorkspaceRuntime {
                package: "dual".to_string(),
                crate_dir: project.path().join("tools/dual"),
                kind: WorkspaceRuntimeKind::Tool,
                binary_names: vec!["dual".to_string()],
                component_assets: None,
            },
        ];
        let error = run(
            &format!("{base}services:\n  dual: {{}}\ntools:\n  dual: {{}}\n"),
            &runtimes,
            &mut Vec::new(),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("both services and tools"), "{error}");
        Ok(())
    }

    #[test]
    fn tools_crate_matching_an_official_identity_still_overrides_it() -> anyhow::Result<()> {
        use phoxal_cli_core::project::train::{WorkspaceRuntime, WorkspaceRuntimeKind};

        let robot = Robot::parse_from_string(
            r#"schema: robot/v0
robot:
  id: bot
  namespace: dev
  motion_limits:
    max_linear_speed_mps: 0.6
    max_angular_speed_radps: 2.0
  kinematic:
    kind: omnidirectional
    actuators: []
    encoders: []
  components: {}
"#,
        )?;
        let project = tempfile::tempdir()?;
        let runtimes = vec![WorkspaceRuntime {
            package: "joypad".to_string(),
            crate_dir: project.path().join("tools/joypad"),
            kind: WorkspaceRuntimeKind::Tool,
            binary_names: vec!["joypad".to_string()],
            component_assets: None,
        }];
        let mut platform_runtimes = Vec::new();
        let mut simulators = Vec::new();
        let mut components = Vec::new();
        // An official `phoxal/tool-joypad` is present in the resolved set.
        let mut tools = vec![ResolvedTool {
            kind: ArtifactKind::Tool,
            name: "tool-joypad".to_string(),
            package: "phoxal/tool-joypad".to_string(),
            requested: "0.1.0".to_string(),
            resolved: "0.1.0".to_string(),
            repo: "phoxal/framework".to_string(),
            asset: "joypad-0.1.0.tar.gz".to_string(),
            binary_name: "phoxal-tool-joypad".to_string(),
            sha256: "0".repeat(64),
            url: None,
            size: None,
            published: true,
            path_override: None,
            train: "0.36.0".to_string(),
            target: host_target_triple(),
        }];
        let resolution = apply_workspace_runtimes(
            &robot,
            project.path(),
            &runtimes,
            &mut platform_runtimes,
            &mut simulators,
            &mut components,
            &mut tools,
            &phoxal_cli_core::project::layout::DriverSelection::All,
        )?;
        assert_eq!(
            tools[0].path_override.as_deref(),
            Some(runtimes[0].crate_dir.as_path())
        );
        assert!(
            resolution
                .path_overrides
                .iter()
                .any(|override_| override_.key == "phoxal/tool-joypad"),
            "official tool override must be recorded"
        );
        Ok(())
    }
}
