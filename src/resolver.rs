use std::path::{Path, PathBuf};

use crate::shell;
use anyhow::{Context, Result, anyhow, bail};
use phoxal::model::robot::{
    RobotV0 as Robot,
    v0::{ArtifactPin, UserService},
};
pub use phoxal_cli_core::project::suite::host_target_triple;
use phoxal_cli_core::project::suite::{
    ArtifactKind, Kind, Suite, artifacts_of_kind, select_artifact,
};
use phoxal_cli_core::project::tooling::{hash_tree, resolve_project_path};

/// The provider every official Phoxal package uses in its provider-qualified
/// `artifacts.pins` / suite `package` identity (`phoxal/service-drive`, ...).
const PHOXAL_PROVIDER: &str = "phoxal";

use phoxal_cli_core::project::resolver::{
    ComponentDriverUnavailable, ResolveOptions, ResolvedComponent, ResolvedComponentPackage,
    ResolvedComponentSource, ResolvedPathOverride, ResolvedPathOverrideKind,
    ResolvedPlatformRuntime, ResolvedRobot, ResolvedTool, ResolvedUserRuntime,
    official_binary_name, tool_emit_apis_id,
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

    let user_runtimes = robot
        .services
        .iter()
        .map(|(name, service)| resolve_user_runtime(project_root, name, service))
        .collect::<Result<Vec<_>>>()?;

    // Git ref → commit SHA resolution: when `resolve_source_commits` is set, a
    // `rev` that is already a full commit SHA resolves with no network, while a
    // tag/branch ref is resolved live via `git ls-remote`. Flows that never read
    // Metadata-only/update flows leave component commit resolution off.
    // Component assets have a separate gate because deploy/check/run do not
    // read non-local asset bundles during planning/staging; live simulate does.
    let mut components = resolve_components(&ComponentResolveContext {
        robot,
        suite: Some(suite),
        train: &train,
        target: &target,
        resolve_source_commits: options.resolve_source_commits,
        resolve_component_asset_commits: options.resolve_component_asset_commits,
        prefer_vendored,
    })?;
    let mut tools = resolve_tools(robot, Some(suite), &train, &tool_target, prefer_vendored)?;
    tools.extend(resolve_native_site_artifacts(
        robot,
        Some(suite),
        &train,
        &tool_target,
        prefer_vendored,
        Kind::Infrastructure,
        ArtifactKind::Infrastructure,
    )?);
    let path_overrides = apply_path_pins(
        &PathPinContext {
            robot,
            project_root,
            resolve_source_commits: options.resolve_source_commits,
        },
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
        suite_profiles: suite.profiles.clone(),
        path_overrides,
    })
}

/// Apply every `artifacts.pins` `Path`/`Git` override to the resolved graph.
/// `Path` pins resolve directly against `project_root`. `Git` pins resolve
/// through the general [`crate::git_artifact`] resolver - the SAME one
/// components use - EXCEPT for a key that belongs to a component: components
/// resolve their own git pins earlier, inside [`resolve_component_package`],
/// and stage them lazily at point-of-use
/// (`component_driver::component_driver_crate_dir`/`component_assets_dir`),
/// so this function skips those keys rather than eagerly cloning them a
/// second time up front. For a non-component key, `resolve_source_commits`
/// gates the same way it does for components: off leaves the pin unapplied
/// rather than touching the network, on resolves the ref to
/// a commit and clones/reuses the shallow checkout.
/// The fixed (non-slice) inputs [`apply_path_pins`] needs, bundled so adding
/// the output mode did not push it over clippy's argument-count lint - same
/// pattern as [`ComponentResolveContext`].
struct PathPinContext<'a> {
    robot: &'a Robot,
    project_root: &'a Path,
    resolve_source_commits: bool,
}

fn apply_path_pins(
    context: &PathPinContext<'_>,
    platform_runtimes: &mut [ResolvedPlatformRuntime],
    simulators: &mut [ResolvedPlatformRuntime],
    components: &mut [ResolvedComponent],
    tools: &mut [ResolvedTool],
) -> Result<Vec<ResolvedPathOverride>> {
    let mut overrides = Vec::new();
    for (key, pin) in &context.robot.artifacts.pins {
        let path = match pin {
            ArtifactPin::Path(pin) => resolve_project_path(context.project_root, &pin.path),
            ArtifactPin::Git(pin) => {
                if is_component_package_key(key, components) {
                    continue;
                }
                let Some(path) =
                    resolve_git_artifact_pin_path(pin, context.resolve_source_commits)?
                else {
                    continue;
                };
                path
            }
        };
        if apply_service_path_pin(key, &path, platform_runtimes, &mut overrides) {
            continue;
        }
        if apply_component_path_pin(key, &path, components, &mut overrides) {
            continue;
        }
        if apply_tool_path_pin(key, &path, tools, &mut overrides) {
            continue;
        }
        if apply_simulator_path_pin(key, &path, simulators, &mut overrides) {
            continue;
        }
        bail!(
            "{}",
            unknown_path_pin_message(key, platform_runtimes, simulators, components, tools)
        );
    }
    overrides.sort_by(|left, right| left.key.cmp(&right.key));
    Ok(overrides)
}

fn is_component_package_key(key: &str, components: &[ResolvedComponent]) -> bool {
    components.iter().any(|component| {
        component
            .assets
            .as_ref()
            .is_some_and(|assets| assets.package == key)
            || component
                .driver
                .as_ref()
                .is_some_and(|driver| driver.package == key)
    })
}

/// Resolve a general (non-component) `Git` artifacts.pin to a local checkout
/// directory: resolve `rev` to a commit (network-gated by
/// `resolve_source_commits`, exactly like [`resolve_component_commit`]), then
/// shallow-clone/reuse it via [`crate::git_artifact::ensure_git_artifact`].
/// `None` when `resolve_source_commits` is off - the pin is left unapplied
/// rather than touching the network (mirrors how an offline component git
/// pin resolves with an empty commit instead of cloning).
fn resolve_git_artifact_pin_path(
    pin: &phoxal::model::robot::v0::ArtifactGitPin,
    resolve_source_commits: bool,
) -> Result<Option<PathBuf>> {
    if !resolve_source_commits {
        return Ok(None);
    }
    let commit = resolve_component_commit(&pin.git, &pin.rev)?;
    let repo_dir = crate::git_artifact::ensure_git_artifact(&pin.git, &commit)?;
    Ok(Some(crate::git_artifact::subdir(
        repo_dir,
        pin.directory.as_deref(),
    )?))
}

fn apply_service_path_pin(
    key: &str,
    path: &Path,
    platform_runtimes: &mut [ResolvedPlatformRuntime],
    overrides: &mut Vec<ResolvedPathOverride>,
) -> bool {
    let Some(runtime) = platform_runtimes
        .iter_mut()
        .find(|runtime| runtime.kind == ArtifactKind::Service && runtime.package == key)
    else {
        return false;
    };
    apply_platform_runtime_path_override(runtime, path);
    overrides.push(ResolvedPathOverride {
        key: key.to_string(),
        kind: ResolvedPathOverrideKind::Service,
        artifact_name: runtime.name.clone(),
        path: path.to_path_buf(),
    });
    true
}

fn apply_component_path_pin(
    key: &str,
    path: &Path,
    components: &mut [ResolvedComponent],
    overrides: &mut Vec<ResolvedPathOverride>,
) -> bool {
    let mut used = false;
    for component in components.iter_mut() {
        if let Some(assets) = component.assets.as_mut()
            && assets.package == key
        {
            assets.path_override = Some(path.to_path_buf());
            used = true;
        }
        if let Some(driver) = component.driver.as_mut()
            && driver.package == key
        {
            driver.path_override = Some(path.to_path_buf());
            used = true;
        }
    }
    if used {
        let kind = if key.ends_with("-driver") {
            ResolvedPathOverrideKind::ComponentDriver
        } else {
            ResolvedPathOverrideKind::ComponentAssets
        };
        overrides.push(ResolvedPathOverride {
            key: key.to_string(),
            kind,
            artifact_name: key.to_string(),
            path: path.to_path_buf(),
        });
    }
    used
}

fn apply_tool_path_pin(
    key: &str,
    path: &Path,
    tools: &mut [ResolvedTool],
    overrides: &mut Vec<ResolvedPathOverride>,
) -> bool {
    let Some(tool) = tools.iter_mut().find(|tool| tool.package == key) else {
        return false;
    };
    tool.path_override = Some(path.to_path_buf());
    tool.asset = format!("path:{}", path.display());
    tool.sha256 = hash_tree(path).unwrap_or_default();
    tool.url = None;
    tool.size = None;
    tool.published = true;
    overrides.push(ResolvedPathOverride {
        key: key.to_string(),
        kind: if tool.kind == ArtifactKind::Infrastructure {
            ResolvedPathOverrideKind::Infrastructure
        } else {
            ResolvedPathOverrideKind::Tool
        },
        artifact_name: tool_emit_apis_id(&tool.name).to_string(),
        path: path.to_path_buf(),
    });
    true
}

fn apply_simulator_path_pin(
    key: &str,
    path: &Path,
    simulators: &mut [ResolvedPlatformRuntime],
    overrides: &mut Vec<ResolvedPathOverride>,
) -> bool {
    let Some(runtime) = simulators
        .iter_mut()
        .find(|runtime| runtime.kind == ArtifactKind::Simulator && runtime.package == key)
    else {
        return false;
    };
    apply_platform_runtime_path_override(runtime, path);
    overrides.push(ResolvedPathOverride {
        key: key.to_string(),
        kind: ResolvedPathOverrideKind::Simulator,
        artifact_name: runtime.name.clone(),
        path: path.to_path_buf(),
    });
    true
}

/// Once a path override replaces a suite-resolved runtime, its suite
/// metadata is moot: the participant's contracts/config come from building
/// its source instead (`crate::check::source_participants_from_resolved`).
fn apply_platform_runtime_path_override(runtime: &mut ResolvedPlatformRuntime, path: &Path) {
    runtime.path_override = Some(path.to_path_buf());
    runtime.artifact_ref = format!("path:{}", path.display());
    runtime.sha256 = None;
    runtime.url = None;
    runtime.size = None;
    runtime.published = true;
    runtime.published_triples = Vec::new();
}

fn unknown_path_pin_message(
    key: &str,
    platform_runtimes: &[ResolvedPlatformRuntime],
    simulators: &[ResolvedPlatformRuntime],
    components: &[ResolvedComponent],
    tools: &[ResolvedTool],
) -> String {
    let used = used_path_pin_keys(platform_runtimes, simulators, components, tools);
    let available = if used.is_empty() {
        "<none>".to_string()
    } else {
        used.join(", ")
    };
    if is_provider_qualified_key(key) {
        format!(
            "unused artifact path pin '{key}': no package with that id is used by the resolved graph; available path-pin ids: {available}"
        )
    } else {
        format!(
            "unknown artifact path pin '{key}': pins must be provider-qualified package ids ('{PHOXAL_PROVIDER}/<name>'); available path-pin ids: {available}"
        )
    }
}

fn is_provider_qualified_key(key: &str) -> bool {
    phoxal::model::robot::v0::is_provider_qualified_pin_key(key)
}

fn used_path_pin_keys(
    platform_runtimes: &[ResolvedPlatformRuntime],
    simulators: &[ResolvedPlatformRuntime],
    components: &[ResolvedComponent],
    tools: &[ResolvedTool],
) -> Vec<String> {
    let mut keys = Vec::new();
    keys.extend(
        platform_runtimes
            .iter()
            .filter(|runtime| runtime.kind == ArtifactKind::Service)
            .map(|runtime| runtime.package.clone()),
    );
    keys.extend(
        components
            .iter()
            .filter_map(|component| component.assets.as_ref())
            .map(|assets| assets.package.clone()),
    );
    keys.extend(
        components
            .iter()
            .filter_map(|component| component.driver.as_ref())
            .map(|driver| driver.package.clone()),
    );
    keys.extend(tools.iter().map(|tool| tool.package.clone()));
    keys.extend(simulators.iter().map(|simulator| simulator.package.clone()));
    keys.sort();
    keys.dedup();
    keys
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
    robot: &Robot,
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
    if matches!(
        robot.artifacts.pins.get(package),
        Some(ArtifactPin::Path(_) | ArtifactPin::Git(_))
    ) {
        return Ok(ResolvedPlatformRuntime {
            name: name.to_string(),
            package: package.to_string(),
            kind,
            version: "source".to_string(),
            artifact_ref: format!("source:{package}"),
            sha256: None,
            url: None,
            size: None,
            published: true,
            published_triples: Vec::new(),
            path_override: None,
            train: train.to_string(),
            target: target.map(str::to_string),
        });
    }
    if prefer_vendored
        && !robot.artifacts.pins.contains_key(package)
        && let Ok(runtime) = vendored_runtime(name, package, kind, train, target)
    {
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

/// Resolve a git `artifacts.pins` ref (component or general) to a concrete
/// commit SHA.
///
/// A ref that is already a full 40-character commit SHA is an explicit pin and
/// is returned as-is with no network access. Any other ref (a tag or branch
/// name) is resolved live via `git ls-remote`; if the network is unavailable the
/// failure is reported with an actionable fix.
fn resolve_component_commit(url: &str, git_ref: &str) -> Result<String> {
    if is_full_commit_sha(git_ref) {
        return Ok(git_ref.to_string());
    }
    resolve_git_ref(url, git_ref).with_context(|| {
        format!(
            "could not resolve git ref '{git_ref}' from {url} without network access. \
             Pin artifacts.pins.<package>.rev to an explicit commit SHA in robot.yaml, \
             or run with network access so `git ls-remote` can resolve the ref."
        )
    })
}

pub(crate) fn is_full_commit_sha(value: &str) -> bool {
    value.len() == 40 && value.chars().all(|byte| byte.is_ascii_hexdigit())
}

pub fn resolve_git_ref(url: &str, git_ref: &str) -> Result<String> {
    let progress = crate::progress::status(format!("resolving git ref {git_ref} from {url}"));
    let result = resolve_git_ref_inner(url, git_ref);
    match &result {
        Ok(_) => {}
        Err(error) => progress.abandon_with_message(format!(
            "failed to resolve git ref {git_ref} from {url}: {error:#}"
        )),
    }
    result
}

fn resolve_git_ref_inner(url: &str, git_ref: &str) -> Result<String> {
    let candidates = [
        format!("refs/tags/{git_ref}^{{}}"),
        format!("refs/tags/{git_ref}"),
        format!("refs/heads/{git_ref}"),
        git_ref.to_string(),
    ];
    for candidate in candidates {
        let output = shell::run_stdout("git", ["ls-remote", url, candidate.as_str()], None)
            .with_context(|| format!("failed to resolve git ref {git_ref} from {url}"))?;
        if let Some(commit) = output
            .split_whitespace()
            .next()
            .filter(|value| !value.is_empty())
        {
            return Ok(commit.to_string());
        }
    }
    Err(anyhow!("git ref {git_ref} does not exist in {url}"))
}

/// Resolve a user-supplied `--target` selector to the full suite target
/// triple official artifacts are suiteed under. Accepts the short arch aliases
/// (`aarch64`/`arm64`, `x86_64`/`amd64`) or a full triple passed through as-is.
/// Official artifacts publish gnu Linux assets, so a bare arch maps to the gnu
/// triple; deploy owns the separate musl cross-build triple.
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

fn resolve_user_runtime(
    project_root: &Path,
    name: &str,
    service: &UserService,
) -> Result<ResolvedUserRuntime> {
    let runtime_dir = resolve_project_path(project_root, &service.path);
    if !runtime_dir.is_dir() {
        bail!(
            "user service '{name}' source dir {} does not exist; user services must have an on-disk source directory to hash/build",
            runtime_dir.display()
        );
    }
    let source_hash = hash_tree(&runtime_dir).with_context(|| {
        format!(
            "failed to hash user service '{name}' source tree at {}",
            runtime_dir.display()
        )
    })?;
    Ok(ResolvedUserRuntime {
        name: name.to_string(),
        path: service.path.clone(),
        source_hash,
    })
}

/// Resolve every `robot.components.<instance>` entry from the flattened
/// `phoxal/component-<id>` artifact. Its assets are used for every instance;
/// its target blob is also used when the instance declares a `driver` block.
///
/// Forks may replace either package slot via `artifacts.pins`; a pin with a
/// `Git`/`Path` form resolves that package from the fork instead of the
/// suite. `resolve_source_commits` gates live `git ls-remote` the same way
/// it always has: metadata-only flows leave it off and skip this entirely.
/// Shared resolution context for [`resolve_component_package`]: the pieces
/// every component package slot (assets or driver) needs, bundled so the
/// per-slot resolver stays under clippy's argument-count lint.
struct ComponentResolveContext<'a> {
    robot: &'a Robot,
    suite: Option<&'a Suite>,
    train: &'a str,
    target: &'a str,
    resolve_source_commits: bool,
    resolve_component_asset_commits: bool,
    prefer_vendored: bool,
}

fn resolve_components(context: &ComponentResolveContext<'_>) -> Result<Vec<ResolvedComponent>> {
    let robot = context.robot;
    let mut components = Vec::new();
    for (instance_name, instance) in &robot.robot.components {
        let component_id = &instance.component;
        let package = format!("{PHOXAL_PROVIDER}/component-{component_id}");

        let has_driver = instance.driver.is_some();
        let assets = match resolve_component_package(
            context,
            &package,
            ArtifactKind::ComponentAssets,
            context.resolve_component_asset_commits,
        ) {
            Ok(assets) => Some(assets),
            // A driverless (passive) component - a mechanical part like a
            // caster wheel - has no official `component_assets` package to
            // resolve; that's valid, not an error. A component with a
            // driver still needs its assets, so that case keeps the
            // existing hard-fail.
            Err(_) if !has_driver => None,
            Err(err) => {
                return Err(err.context(format!(
                    "robot.components.{instance_name}.component '{component_id}' failed to resolve its component_assets package"
                )));
            }
        };

        let driver = if has_driver {
            match resolve_component_package(
                context,
                &package,
                ArtifactKind::ComponentDriver,
                context.resolve_source_commits,
            ) {
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
/// `component_driver`) for `package`: an `artifacts.pins` entry takes
/// precedence (`Git`/`Path` pin form), otherwise it resolves
/// from the suite. A suite resolution also captures the matched entry's
/// built artifact for the needed scope (assets or the
/// resolved target triple for drivers) into `suite_runtime`, exactly like a
/// service/simulator captures `artifact_ref`/`sha256`/`published` - see
/// [`resolved_runtime_from_artifact_entry`]. If the entry exists but has no
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
    resolve_git_ref: bool,
) -> Result<ResolvedComponentPackage> {
    if let Some(pin @ (ArtifactPin::Path(_) | ArtifactPin::Git(_))) =
        context.robot.artifacts.pins.get(package)
    {
        return resolve_pinned_component_package(package, kind, pin, resolve_git_ref);
    }

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

fn resolve_pinned_component_package(
    package: &str,
    kind: ArtifactKind,
    pin: &ArtifactPin,
    resolve_git_ref: bool,
) -> Result<ResolvedComponentPackage> {
    let source = match pin {
        ArtifactPin::Path(pin) => ResolvedComponentSource::Path {
            path: pin.path.clone(),
        },
        ArtifactPin::Git(pin) => {
            // Resolve the commit live. A `rev` that is already a full commit
            // SHA needs no network; a tag/branch ref is resolved via
            // `git ls-remote`. Flows that never read this package's local
            // files leave `resolve_git_ref` off and skip this entirely.
            let commit = if resolve_git_ref {
                resolve_component_commit(&pin.git, &pin.rev)?
            } else {
                String::new()
            };
            ResolvedComponentSource::Git {
                git: pin.git.clone(),
                rev: commit,
                directory: pin.directory.clone(),
            }
        }
    };
    Ok(ResolvedComponentPackage {
        package: package.to_string(),
        kind,
        source,
        path_override: None,
        // Source pins deliberately bypass the suite and carry no suite
        // runtime.
        suite_runtime: None,
    })
}

fn resolve_tools(
    robot: &Robot,
    suite: Option<&Suite>,
    train: &str,
    target: &str,
    prefer_vendored: bool,
) -> Result<Vec<ResolvedTool>> {
    resolve_native_site_artifacts(
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
fn resolve_native_site_artifacts(
    robot: &Robot,
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
            if matches!(
                robot.artifacts.pins.get(package),
                Some(ArtifactPin::Path(_) | ArtifactPin::Git(_))
            ) {
                return Ok(ResolvedTool {
                    kind,
                    name: format!("{}-{artifact_name}", kind.emit_apis_kind()),
                    package: package.to_string(),
                    requested: "source".to_string(),
                    resolved: "source".to_string(),
                    repo: "source".to_string(),
                    asset: format!("source:{package}"),
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
            if prefer_vendored
                && !robot.artifacts.pins.contains_key(package)
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
    use phoxal_cli_core::project::resolver::{load_robot, load_robot_with_extras};
    use phoxal_cli_core::project::suite::{
        fixture_component_assets_entry_for_tests, fixture_contract_for_tests,
        fixture_service_entry_for_tests, fixture_suite_for_tests,
    };

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

    #[test]
    fn resolve_without_source_commits_leaves_git_component_commits_empty() -> anyhow::Result<()> {
        // Flows that never read component commits resolve
        // with `resolve_source_commits: false` and must NOT run `git ls-remote`.
        // A git component pin is resolved with an empty commit; if resolution
        // tried to reach the network it would either hang or fail, so an empty
        // commit proves no ls-remote was attempted.
        let _phoxal_home = ScratchPhoxalHome::new()?;
        let robot = Robot::parse_from_string(GIT_COMPONENT_ROBOT)?;
        let suite = test_suite();
        let resolved = resolve(
            &robot,
            std::path::Path::new("."),
            Some(&suite),
            ResolveOptions {
                resolve_source_commits: false,
                resolve_component_asset_commits: false,
                ..ResolveOptions::default()
            },
        )?;

        let git_component = resolved
            .components
            .iter()
            .find(|component| component.source_name == "ddsm115")
            .expect("ddsm115 component resolved");
        match &git_component
            .assets
            .as_ref()
            .expect("ddsm115 has a driver; assets must resolve")
            .source
        {
            ResolvedComponentSource::Git { rev, .. } => {
                assert!(
                    rev.is_empty(),
                    "offline resolve must leave the git commit empty (no ls-remote), got {rev:?}"
                );
            }
            other => panic!("expected a git component source, got {other:?}"),
        }
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
artifacts: {}
"#,
        )?;
        let suite = test_suite();
        let resolved = resolve(
            &robot,
            std::path::Path::new("."),
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

    const GIT_COMPONENT_ROBOT: &str = r#"schema: robot/v0
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
artifacts:
  pins:
    phoxal/component-ddsm115:
      git: https://github.com/phoxal/framework
      rev: main
      directory: component/ddsm115
"#;

    #[test]
    fn explicit_commit_sha_tag_resolves_without_network() -> anyhow::Result<()> {
        // A `rev` that is already a full commit SHA is an explicit pin: it must
        // resolve with no network (no `git ls-remote`), so a live-resolution
        // flow works offline when components are pinned to a SHA.
        let _phoxal_home = ScratchPhoxalHome::new()?;
        let sha = "0123456789abcdef0123456789abcdef01234567";
        let robot = Robot::parse_from_string(
            &GIT_COMPONENT_ROBOT.replace("rev: main", &format!("rev: {sha}")),
        )?;
        let suite = test_suite();
        let resolved = resolve(
            &robot,
            std::path::Path::new("."),
            Some(&suite),
            ResolveOptions {
                resolve_source_commits: true,
                resolve_component_asset_commits: true,
                ..ResolveOptions::default()
            },
        )?;

        let git_component = resolved
            .components
            .iter()
            .find(|component| component.source_name == "ddsm115")
            .expect("ddsm115 component resolved");
        match &git_component
            .assets
            .as_ref()
            .expect("ddsm115 has a driver; assets must resolve")
            .source
        {
            ResolvedComponentSource::Git { rev, .. } => {
                assert_eq!(rev, sha);
            }
            other => panic!("expected a git component source, got {other:?}"),
        }
        Ok(())
    }

    #[test]
    fn driverless_component_with_no_official_assets_package_resolves_with_none_assets()
    -> anyhow::Result<()> {
        // A passive mechanical component (e.g. robot-v1's `front_caster`,
        // `component: passive_caster`) declares no `driver:` block and has
        // no official `phoxal/component-<id>` assets package in the suite
        // at all - that's a valid, real-world configuration. Resolution
        // must succeed with `assets: None`, not hard-fail (this was the
        // reported bug: `check` failed with "expected package
        // phoxal/component-passive_caster is absent from snapshot ...").
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
artifacts: {}
"#,
        )?;
        // `test_suite()` carries no `phoxal/component-passive_caster`
        // entry at all - exactly the "absent from snapshot" case the bug
        // report hit.
        let suite = test_suite();
        let resolved = resolve(
            &robot,
            std::path::Path::new("."),
            Some(&suite),
            ResolveOptions {
                resolve_source_commits: false,
                resolve_component_asset_commits: false,
                ..ResolveOptions::default()
            },
        )?;

        let caster = resolved
            .components
            .iter()
            .find(|component| component.instance == "front_caster")
            .expect("front_caster component resolved");
        assert!(!caster.has_driver);
        assert!(caster.driver.is_none());
        assert!(
            caster.assets.is_none(),
            "a driverless component with no official assets package must resolve with \
             assets: None, got {:?}",
            caster.assets
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
artifacts: {}
"#,
        )?;
        let suite = test_suite();
        let error = resolve(
            &robot,
            std::path::Path::new("."),
            Some(&suite),
            ResolveOptions {
                resolve_source_commits: false,
                resolve_component_asset_commits: false,
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
    fn full_commit_sha_is_detected() {
        assert!(is_full_commit_sha(
            "0123456789abcdef0123456789abcdef01234567"
        ));
        assert!(!is_full_commit_sha("main"));
        assert!(!is_full_commit_sha("v0.3.0"));
        // 39 chars (too short) and a non-hex char are both rejected.
        assert!(!is_full_commit_sha(
            "0123456789abcdef0123456789abcdef0123456"
        ));
        assert!(!is_full_commit_sha(
            "0123456789abcdef0123456789abcdef0123456z"
        ));
    }

    #[test]
    fn load_robot_tolerates_user_service_config() -> anyhow::Result<()> {
        // The CLI threads `services.<name>.config` through
        // `RobotManifestExtras` as a side channel; `load_robot` must strip it
        // so every command accepts a manifest that declares typed config.
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
    path: runtimes/brain
    config:
      gain: 0.5
"#,
        )?;

        // Plain load_robot must parse it despite the config key the typed model
        // does not know about.
        let robot = load_robot(&path)?;
        assert!(robot.services.contains_key("brain"));

        let loaded = load_robot_with_extras(&path)?;
        assert!(loaded.robot.services.contains_key("brain"));
        assert_eq!(
            loaded.extras.user_runtime_config("brain"),
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

        let loaded = load_robot_with_extras(&path)?;
        assert_eq!(
            loaded.robot.router.config,
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
  mission-service:
    path: runtimes/mission
  left_drive:
    path: runtimes/left_drive
"#,
        )?;

        let error = load_robot_with_extras(&path).expect_err("launch ids should be checked");
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
}
