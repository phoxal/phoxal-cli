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
    ResolvedPlatformRuntime, ResolvedRobot, ResolvedTool, ResolvedUserRuntime,
    official_binary_name,
};

pub fn resolve(
    robot: &Robot,
    project_root: &Path,
    suite: Option<&Suite>,
    options: ResolveOptions,
) -> Result<ResolvedRobot> {
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
    let platform_names = artifacts_of_kind(suite, Kind::Service)
        .into_iter()
        .map(|artifact| short_name(&artifact.id, Kind::Service))
        .collect::<Vec<_>>();
    let platform_names = platform_names
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>();
    // Finding A3: robot.yaml structural/schema validation always genuinely
    // runs (never conditionally skipped like download/build), so it always
    // gets its own truthful "validate" phase rather than the old synthetic
    // single "Preparing" phase.
    crate::session::diagnostics::run_phase(
        phoxal_cli_core::session::event::PhaseId::new("validate"),
        "Validating robot.yaml".to_string(),
        || {
            robot
                .validate_with(&platform_names)
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
    let mut simulators = resolve_suite_entries(
        robot,
        suite,
        Kind::Simulator,
        ArtifactKind::Simulator,
        &tool_target,
        prefer_vendored,
    )?;

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
    let (user_runtimes, path_overrides) = apply_workspace_runtimes(
        robot,
        project_root,
        &project.runtimes,
        &mut platform_runtimes,
        &mut simulators,
        &mut components,
        &mut tools,
    )?;

    Ok(ResolvedRobot {
        robot: robot.clone(),
        train,
        target,
        platform_runtimes,
        simulators,
        user_runtimes,
        components,
        tools,
        path_overrides,
    })
}

fn apply_workspace_runtimes(
    robot: &Robot,
    project_root: &Path,
    runtimes: &[phoxal_cli_core::project::train::WorkspaceRuntime],
    platform_runtimes: &mut [ResolvedPlatformRuntime],
    _simulators: &mut [ResolvedPlatformRuntime],
    components: &mut [ResolvedComponent],
    tools: &mut [ResolvedTool],
) -> Result<(Vec<ResolvedUserRuntime>, Vec<ResolvedPathOverride>)> {
    use phoxal_cli_core::project::train::WorkspaceRuntimeKind;

    let mut user_runtimes = Vec::new();
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
                } else {
                    user_runtimes.push(ResolvedUserRuntime {
                        name: logical_name,
                        path: relative,
                        source_hash: hash_tree(&runtime.crate_dir)?,
                    });
                }
            }
            WorkspaceRuntimeKind::Tool => {
                let official_package = format!("phoxal/tool-{logical_name}");
                // A `tools/` workspace crate whose name matches an official
                // tool identity still overrides that official binary. A crate
                // that matches NO official identity is a precise authoring
                // error: additional (non-official) user tools were removed from
                // the runtime model (#936). User code in the graph is user
                // services plus component drivers, period.
                let Some(official) = tools
                    .iter_mut()
                    .find(|entry| entry.package == official_package)
                else {
                    bail!(
                        "workspace crate tools/{logical_name} matches no official tool identity; \
                         additional user tools are not part of the runtime model - make it a \
                         service instead (move it to services/{logical_name} and declare it \
                         under `services:` in robot.yaml)"
                    );
                };
                official.path_override = Some(runtime.crate_dir.clone());
                official.asset = format!("path:{}", runtime.crate_dir.display());
                overrides.push(ResolvedPathOverride {
                    key: official_package,
                    kind: ResolvedPathOverrideKind::Tool,
                    artifact_name: logical_name,
                    path: runtime.crate_dir.clone(),
                });
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
                    component.assets = Some(ResolvedComponentPackage {
                        package: format!("workspace/component-{logical_name}"),
                        kind: ArtifactKind::ComponentAssets,
                        source: ResolvedComponentSource::Path {
                            path: assets_dir.clone(),
                        },
                        path_override: Some(assets_dir.clone()),
                        suite_runtime: None,
                    });
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
                        component.driver = Some(ResolvedComponentPackage {
                            package: format!("workspace/component-{logical_name}"),
                            kind: ArtifactKind::ComponentDriver,
                            source: ResolvedComponentSource::Path {
                                path: runtime.crate_dir.clone(),
                            },
                            path_override: Some(runtime.crate_dir.clone()),
                            suite_runtime: None,
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
        "robot.yaml service configuration has no matching services/ workspace crate: {}",
        missing
            .into_iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join(", ")
    );
    overrides.sort_by(|left, right| left.key.cmp(&right.key));
    Ok((user_runtimes, overrides))
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
                assets: Some(ResolvedComponentPackage {
                    package: format!("workspace/component-{component_id}"),
                    kind: ArtifactKind::ComponentAssets,
                    source: ResolvedComponentSource::Path {
                        path: assets_dir.clone(),
                    },
                    path_override: Some(assets_dir.clone()),
                    suite_runtime: None,
                }),
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
        // robot-v1's passive_caster, is a workspace crate) and hid unknown
        // packages, unsupported targets, and malformed suite state behind a
        // silent "assetless" (#936). If genuinely assetless suite components
        // become real, the suite/catalog models that explicitly.
        let assets = Some(
            resolve_component_package(context, &package, ArtifactKind::ComponentAssets).map_err(
                |err| {
                    err.context(format!(
                        "robot.components.{instance_name}.component '{component_id}' failed to resolve its component_assets package"
                    ))
                },
            )?,
        );

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
                    name: format!("{}-{artifact_name}", kind.emit_apis_kind()),
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
                name: format!("{}-{artifact_name}", kind.emit_apis_kind()),
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
        fixture_component_assets_entry_for_tests, fixture_contract_for_tests,
        fixture_service_entry_for_tests, fixture_suite_for_tests,
    };
    use std::path::PathBuf;

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
                vec![fixture_contract_for_tests("v0.1::drive::Target", "publish")],
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
        anyhow::ensure!(
            std::process::Command::new("cargo")
                .arg("generate-lockfile")
                .current_dir(root.path())
                .status()?
                .success(),
            "failed to generate fixture Cargo.lock"
        );
        Ok(root)
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
        // Since #945, the real driverless component (robot-v1's
        // `passive_caster`) is a workspace assets crate and resolves through
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
    fn unmatched_tools_crate_is_a_precise_make_it_a_service_error() -> anyhow::Result<()> {
        use phoxal_cli_core::project::train::{WorkspaceRuntime, WorkspaceRuntimeKind};

        // A `tools/` workspace crate that matches NO official tool identity is a
        // resolve-time error telling the author to make it a service - additional
        // user tools are no longer part of the runtime model (#936).
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
            package: "custom".to_string(),
            crate_dir: project.path().join("tools/custom"),
            kind: WorkspaceRuntimeKind::Tool,
            binary_names: vec!["custom".to_string()],
            component_assets: None,
        }];
        let mut platform_runtimes = Vec::new();
        let mut simulators = Vec::new();
        let mut components = Vec::new();
        let mut tools = Vec::new();
        let error = apply_workspace_runtimes(
            &robot,
            project.path(),
            &runtimes,
            &mut platform_runtimes,
            &mut simulators,
            &mut components,
            &mut tools,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("tools/custom"), "{error}");
        assert!(error.contains("make it a service"), "{error}");
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
        let (_user_runtimes, overrides) = apply_workspace_runtimes(
            &robot,
            project.path(),
            &runtimes,
            &mut platform_runtimes,
            &mut simulators,
            &mut components,
            &mut tools,
        )?;
        assert_eq!(
            tools[0].path_override.as_deref(),
            Some(runtimes[0].crate_dir.as_path())
        );
        assert!(
            overrides
                .iter()
                .any(|override_| override_.key == "phoxal/tool-joypad"),
            "official tool override must be recorded"
        );
        Ok(())
    }
}
