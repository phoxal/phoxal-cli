use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use phoxal::model::robot::{
    RobotV0 as Robot,
    v0::{ArtifactPin, UserService},
};
use serde_json::Value;

use crate::catalog::{
    ArtifactKind, Catalog, OFFICIAL_INFRASTRUCTURE, OFFICIAL_SERVICES, OFFICIAL_SIMULATORS,
    OFFICIAL_TOOLS, SelectionChannel, select_artifact, selection_channel,
};
use crate::shell;
use crate::utils::{hash_tree, resolve_project_path};

/// The provider every official Phoxal package uses in its provider-qualified
/// `artifacts.pins` / catalog `package` identity (`phoxal/service-drive`, ...).
const PHOXAL_PROVIDER: &str = "phoxal";

const ROBOT_FILE: &str = "robot.yaml";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolveOptions {
    /// Move unpinned channel selections to the supplied head snapshot. Only
    /// `phoxal update` sets this; normal commands prefer `active` symlinks.
    pub refresh_channel_head: bool,
    /// Emit the once-per-invocation artifact update notice. Watch rebuilds
    /// disable this so the top-level invocation never repeats it.
    pub emit_update_notice: bool,
    /// Resolve git component `tag` → `commit`. A `tag` that is already a full
    /// commit SHA resolves with no network; a tag/branch ref is resolved live
    /// via `git ls-remote`. Flows that need to locate/stage component driver
    /// sources (`check`, `run --watch`, simulate, `deploy`) set this;
    /// metadata-only/update flows leave it off so they stay offline for git refs.
    pub resolve_source_commits: bool,
    /// Resolve git-backed `component_assets` refs to commits. Most commands do
    /// not read non-local asset bundles while rendering plans/payload metadata,
    /// so they leave this off to avoid staging-time network access; live
    /// simulation turns it on because Webots world generation needs the asset
    /// files locally.
    pub resolve_component_asset_commits: bool,
    /// Override the official service/driver target triple. Deploy probes the
    /// robot arch and resolves catalog assets for that Linux triple instead of
    /// the host.
    pub official_target_triple: Option<String>,
    /// Override native tool asset target triple. Host-native run/sim use the
    /// host triple; deploy ships robot-native tools.
    pub tool_target_triple: Option<String>,
    /// The session's output mode, threaded into a git-ref resolution
    /// spinner (`resolve_git_ref`) - no process-global mode cell. Defaults
    /// to [`OutputMode::Plain`](crate::output_mode::OutputMode), the safe
    /// non-drawing choice, for callers (mostly tests) with no real session
    /// mode to report.
    pub output_mode: crate::output_mode::OutputMode,
}

impl Default for ResolveOptions {
    fn default() -> Self {
        Self {
            refresh_channel_head: false,
            emit_update_notice: true,
            resolve_source_commits: true,
            resolve_component_asset_commits: true,
            official_target_triple: None,
            tool_target_triple: None,
            output_mode: crate::output_mode::OutputMode::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedRobot {
    pub robot: Robot,
    pub channel: SelectionChannel,
    pub target: String,
    pub catalog_snapshot: Option<String>,
    pub platform_runtimes: Vec<ResolvedPlatformRuntime>,
    pub simulators: Vec<ResolvedPlatformRuntime>,
    pub user_runtimes: Vec<ResolvedUserRuntime>,
    pub components: Vec<ResolvedComponent>,
    pub tools: Vec<ResolvedTool>,
    pub path_overrides: Vec<ResolvedPathOverride>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct RobotManifestExtras {
    pub catalog_source: Option<PathBuf>,
    pub user_runtimes: BTreeMap<String, UserRuntimeManifestExtras>,
}

impl RobotManifestExtras {
    #[must_use]
    pub fn user_runtime_config(&self, runtime_name: &str) -> Option<&Value> {
        self.user_runtimes
            .get(runtime_name)
            .and_then(|runtime| runtime.config.as_ref())
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct UserRuntimeManifestExtras {
    pub config: Option<Value>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LoadedRobot {
    pub robot: Robot,
    pub extras: RobotManifestExtras,
}

/// One resolved official platform artifact (a service or a simulator). The
/// public identity is the provider-qualified `package` id
/// (`phoxal/service-drive`); there is no separate `artifact_id` (docs #21).
///
/// Location and integrity come from the catalog. Contract/config metadata is
/// always extracted from the staged binary.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedPlatformRuntime {
    pub name: String,
    pub package: String,
    pub kind: ArtifactKind,
    pub version: String,
    pub artifact_ref: String,
    pub sha256: Option<String>,
    pub url: Option<String>,
    pub size: Option<u64>,
    /// Whether the catalog has a built [`crate::catalog::Artifact`] (tarball)
    /// for the resolved target triple. `false` for a metadata-only / not yet
    /// published entry - resolution still succeeds (the package is real and
    /// versioned), but there is nothing to fetch yet.
    pub published: bool,
    /// Every target triple the catalog has a built tarball for, for
    /// diagnostics (`ensure_catalog_availability`, `generations status`).
    pub published_triples: Vec<String>,
    pub path_override: Option<PathBuf>,
    /// The channel snapshot this entry belongs to.
    pub channel: SelectionChannel,
    /// The target triple this entry was resolved/built for. `None` identifies
    /// the catalog's distinct component-assets blob.
    pub target: Option<String>,
}

impl ResolvedPlatformRuntime {
    /// The selected official service artifact identifier.
    #[must_use]
    pub fn artifact_ref(&self) -> &str {
        &self.artifact_ref
    }

    #[must_use]
    pub fn source_path(&self) -> Option<&Path> {
        self.path_override.as_deref()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedUserRuntime {
    pub name: String,
    pub path: PathBuf,
    pub source_hash: String,
}

/// One resolved `robot.components.<instance>` entry: the logical component id
/// (`component: <id>`) resolves to an always-present `component_assets`
/// package and an optional `component_driver` package - present only when the
/// instance declares a `driver` block AND a matching driver package exists in
/// the resolved graph (docs #21). Driverless components are valid: they still
/// carry assets and may be simulated, but never launch a hardware driver.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedComponent {
    pub instance: String,
    /// The logical component id (`component: <id>` in `robot.yaml`).
    pub source_name: String,
    /// The resolved `component_assets` package. `Some` when an official
    /// `phoxal/component-<id>` assets package resolved for this component.
    /// `None` for a driverless (passive) component - e.g. a mechanical
    /// mount like a caster wheel - whose assets package doesn't exist in
    /// the catalog; that's a valid configuration, not an error. A
    /// component that declares a `driver:` block always has `Some` here
    /// (a missing assets package for a driven component is still a hard
    /// resolution failure).
    pub assets: Option<ResolvedComponentPackage>,
    /// The resolved `component_driver` package. Present only when the
    /// instance declares `driver` and a driver package resolves for this
    /// component; see [`ComponentDriverUnavailable`].
    pub driver: Option<ResolvedComponentPackage>,
    /// Whether the instance declares a `driver:` block in `robot.yaml`. This
    /// is the manifest-level intent; `driver.is_some()` is whether a matching
    /// package actually resolved for it.
    pub has_driver: bool,
}

impl ResolvedComponent {
    #[must_use]
    pub fn driver_path_override(&self) -> Option<&Path> {
        self.driver
            .as_ref()
            .and_then(|driver| driver.path_override())
    }
}

/// One resolved component package (either the `component_assets` or the
/// `component_driver` half of a component instance).
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedComponentPackage {
    /// The provider-qualified package id (`phoxal/component-ddsm115`).
    pub package: String,
    pub kind: ArtifactKind,
    pub source: ResolvedComponentSource,
    pub path_override: Option<PathBuf>,
    /// Present exactly when `source == Catalog` and the catalog resolved a
    /// matching entry for the needed scope (assets or `context.target`).
    /// Carries the same shape a
    /// service/simulator resolves to ([`ResolvedPlatformRuntime`]) so
    /// components stage through the identical native-artifact machinery
    /// (`native_artifacts::NativeArtifactDescriptor`) instead of a parallel
    /// bespoke path. `None` for `Path`/`Git` sources.
    pub catalog_runtime: Option<ResolvedPlatformRuntime>,
}

impl ResolvedComponentPackage {
    #[must_use]
    pub fn path_override(&self) -> Option<&Path> {
        self.path_override.as_deref()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolvedComponentSource {
    Git {
        git: String,
        rev: String,
        /// Subdirectory within the repository holding the component
        /// definition. `None` means the repository root.
        directory: Option<PathBuf>,
    },
    Path {
        path: PathBuf,
    },
    /// Resolves from the official artifact catalog (no fork pin for this
    /// package); staged from a catalog release asset.
    Catalog,
}

/// A resolved native site artifact (`tool-bus`, `tool-joypad`, or
/// `infrastructure-router`). `name` is the short,
/// launch-safe kind-qualified id used for participant/site ids, systemd unit
/// names, and env var keys (`SITE_TOOL_BUS` etc.); `package` is the
/// canonical provider-qualified identity (`phoxal/tool-bus`) used for
/// catalog lookups and native-artifact provisioning.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedTool {
    pub kind: ArtifactKind,
    pub name: String,
    pub package: String,
    pub requested: String,
    pub resolved: String,
    pub repo: String,
    pub asset: String,
    pub binary_name: String,
    pub sha256: String,
    pub url: Option<String>,
    pub size: Option<u64>,
    /// Whether the catalog has a built [`crate::catalog::Artifact`] (tarball)
    /// for the resolved target triple; `false` for a metadata-only / not yet
    /// published entry, in which case `sha256` is a placeholder
    /// (`"0".repeat(64)`) rather than a real digest - mirrors
    /// [`ResolvedPlatformRuntime::published`].
    pub published: bool,
    pub path_override: Option<PathBuf>,
    /// The channel this entry was selected on; see
    /// [`ResolvedPlatformRuntime::channel`].
    pub channel: SelectionChannel,
    /// The target triple this entry was resolved/built for; see
    /// [`ResolvedPlatformRuntime::target`].
    pub target: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolvedPathOverrideKind {
    Service,
    ComponentAssets,
    ComponentDriver,
    Tool,
    Simulator,
    Infrastructure,
}

impl ResolvedPathOverrideKind {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Service => "service",
            Self::ComponentAssets => "component_assets",
            Self::ComponentDriver => "component_driver",
            Self::Tool => "tool",
            Self::Simulator => "simulator",
            Self::Infrastructure => "infrastructure",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedPathOverride {
    pub key: String,
    pub kind: ResolvedPathOverrideKind,
    pub artifact_name: String,
    pub path: PathBuf,
}

/// A named diagnostic: an instance declares `driver:` but the resolved graph
/// has no matching `component_driver` package for its component (docs #21).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComponentDriverUnavailable {
    pub instance: String,
    pub component: String,
}

impl std::fmt::Display for ComponentDriverUnavailable {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "ComponentDriverUnavailable: robot.components.{}.driver is declared but component '{}' has no {PHOXAL_PROVIDER}/component-{}-driver package in the resolved graph",
            self.instance, self.component, self.component
        )
    }
}

impl std::error::Error for ComponentDriverUnavailable {}

pub fn discover_robot_yaml(start: &Path) -> Result<PathBuf> {
    let mut cursor = if start.is_file() {
        start
            .parent()
            .ok_or_else(|| anyhow!("cannot discover robot.yaml above {}", start.display()))?
            .to_path_buf()
    } else {
        start.to_path_buf()
    };

    loop {
        let candidate = cursor.join(ROBOT_FILE);
        if candidate.is_file() {
            return Ok(candidate);
        }
        if !cursor.pop() {
            bail!("failed to discover robot.yaml above {}", start.display());
        }
    }
}

pub fn load_robot(path: &Path) -> Result<Robot> {
    // Delegate to the extras-aware loader so EVERY command tolerates the
    // `services.<name>.config` keys as a CLI-side side channel: they are
    // stripped before the typed parse and threaded through
    // `RobotManifestExtras`. Commands that don't need the extras just discard
    // them.
    load_robot_with_extras(path).map(|loaded| loaded.robot)
}

pub fn load_robot_with_extras(path: &Path) -> Result<LoadedRobot> {
    let contents = fs::read_to_string(path)
        .with_context(|| format!("failed to read robot file {}", path.display()))?;
    let mut yaml = serde_yaml::from_str::<serde_yaml::Value>(&contents)
        .with_context(|| format!("failed to parse robot file {}", path.display()))?;
    ensure_no_base_path_pins(&yaml, path)?;
    parse_robot_value_with_extras(&mut yaml, path)
}

pub fn load_robot_with_extras_and_overlays(
    path: &Path,
    overlays: &[String],
) -> Result<LoadedRobot> {
    let contents = fs::read_to_string(path)
        .with_context(|| format!("failed to read robot file {}", path.display()))?;
    let mut yaml = serde_yaml::from_str::<serde_yaml::Value>(&contents)
        .with_context(|| format!("failed to parse robot file {}", path.display()))?;
    ensure_no_base_path_pins(&yaml, path)?;

    for overlay in overlays {
        validate_overlay_name(overlay)?;
        let overlay_path = path.with_file_name(format!("robot.{overlay}.yaml"));
        let overlay_contents = fs::read_to_string(&overlay_path)
            .with_context(|| format!("failed to read overlay {}", overlay_path.display()))?;
        let overlay_yaml = serde_yaml::from_str::<serde_yaml::Value>(&overlay_contents)
            .with_context(|| format!("failed to parse overlay {}", overlay_path.display()))?;
        merge_yaml_overlay(&mut yaml, overlay_yaml, &mut Vec::new());
    }

    parse_robot_value_with_extras(&mut yaml, path)
}

fn ensure_no_base_path_pins(yaml: &serde_yaml::Value, path: &Path) -> Result<()> {
    let Some(pins) = yaml
        .as_mapping()
        .and_then(|root| root.get("artifacts"))
        .and_then(serde_yaml::Value::as_mapping)
        .and_then(|artifacts| artifacts.get("pins"))
        .and_then(serde_yaml::Value::as_mapping)
    else {
        return Ok(());
    };

    // A path pin whose target stays INSIDE the project (a robot-local component
    // checked into the robot repo, e.g. `./components/passive_caster`) is
    // permanent, reproducible robot content and is allowed in the base manifest.
    // Only a path that ESCAPES the project (absolute, lexically climbing out
    // with `..`, or resolving outside through a symlink) is a dev override and
    // must live in a `robot.<env>.yaml` overlay so production manifests stay
    // catalog/release based.
    let project_root = path.parent().unwrap_or_else(|| Path::new("."));
    let escaping_pins = pins
        .iter()
        .filter_map(|(key, value)| {
            let path_key = serde_yaml::Value::String("path".to_string());
            let pin_path = value
                .as_mapping()
                .and_then(|mapping| mapping.get(&path_key))
                .and_then(serde_yaml::Value::as_str)?;
            path_pin_escapes_project(Path::new(pin_path), project_root)
                .then(|| key.as_str().unwrap_or("<non-string>").to_string())
        })
        .collect::<Vec<_>>();
    if escaping_pins.is_empty() {
        return Ok(());
    }
    bail!(
        "{path}: artifacts.pins path overrides that point outside the project are dev-overlay only; move {} to robot.<env>.yaml and load it with --env <env> (in-project component paths are allowed in the base manifest)",
        escaping_pins.join(", "),
        path = path.display()
    )
}

/// Whether a pin `path` resolves outside the project root. Reject obvious lexical
/// escapes first, then compare canonical paths when the target already exists so
/// symlinks cannot carry a base-manifest pin outside the project. A missing target
/// falls back to the lexical result so an in-project path may be created later.
fn path_pin_escapes_project(pin_path: &Path, project_root: &Path) -> bool {
    if path_pin_lexically_escapes_project(pin_path) {
        return true;
    }

    let Ok(canonical_root) = project_root.canonicalize() else {
        return false;
    };
    let resolved = resolve_project_path(project_root, pin_path);
    let Ok(canonical_target) = resolved.canonicalize() else {
        return false;
    };
    !canonical_target.starts_with(canonical_root)
}

fn path_pin_lexically_escapes_project(pin_path: &Path) -> bool {
    use std::path::Component;
    if pin_path.is_absolute() {
        return true;
    }
    let mut depth: i32 = 0;
    for component in pin_path.components() {
        match component {
            Component::ParentDir => {
                depth -= 1;
                if depth < 0 {
                    return true;
                }
            }
            Component::Normal(_) => depth += 1,
            Component::CurDir => {}
            // A rooted/prefix component means absolute-like; treat as escaping.
            Component::RootDir | Component::Prefix(_) => return true,
        }
    }
    false
}

fn parse_robot_value_with_extras(yaml: &mut serde_yaml::Value, path: &Path) -> Result<LoadedRobot> {
    let (extras, _) = take_manifest_extras(yaml, path)?;
    translate_nightly_channel_for_framework(yaml, path)?;
    let sanitized = serde_yaml::to_string(&yaml)
        .with_context(|| format!("failed to prepare {}", path.display()))?;
    let robot = Robot::read_from_string(&sanitized)?;
    validate_launch_participant_ids(&robot, path)?;

    Ok(LoadedRobot { robot, extras })
}

fn translate_nightly_channel_for_framework(
    yaml: &mut serde_yaml::Value,
    path: &Path,
) -> Result<()> {
    let Some(channel) = yaml
        .as_mapping_mut()
        .and_then(|root| root.get_mut("artifacts"))
        .and_then(serde_yaml::Value::as_mapping_mut)
        .and_then(|artifacts| artifacts.get_mut("channel"))
    else {
        return Ok(());
    };
    match channel.as_str() {
        Some("nightly") => {
            // Framework main has not yet renamed its internal variant. Keep
            // this translation confined to deserialization; CLI UX never
            // accepts or prints the removed preview spelling.
            *channel = serde_yaml::Value::String("preview".to_string());
            Ok(())
        }
        Some("preview") => bail!(
            "{} uses removed artifacts.channel `preview`; replace it with `nightly`",
            path.display()
        ),
        _ => Ok(()),
    }
}

fn validate_launch_participant_ids(robot: &Robot, path: &Path) -> Result<()> {
    let mut errors = Vec::new();
    for name in robot.services.keys() {
        if !is_launch_id(name) {
            errors.push(format!(
                "services.{name} in {} must use only [a-z0-9_]; '-' is reserved as a launch separator",
                path.display()
            ));
        }
    }
    for instance in robot.robot.components.keys() {
        if !is_launch_id(instance) {
            errors.push(format!(
                "robot.components.{instance} in {} must use only [a-z0-9_]; '-' is reserved as a launch separator",
                path.display()
            ));
        }
        if robot.services.contains_key(instance) {
            errors.push(format!(
                "services.{instance} collides with robot.components.{instance}; participant ids must be unique",
            ));
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        bail!("Robot launch id errors:\n{}", errors.join("\n"))
    }
}

pub(crate) fn is_launch_id(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
}

fn validate_overlay_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.contains('/')
        || name.contains('\\')
        || name == "."
        || name == ".."
        || name.chars().any(char::is_whitespace)
    {
        bail!(
            "overlay name '{name}' is invalid; use a simple name such as `prod` for robot.prod.yaml"
        );
    }
    Ok(())
}

fn merge_yaml_overlay(
    base: &mut serde_yaml::Value,
    overlay: serde_yaml::Value,
    path: &mut Vec<String>,
) {
    if is_replace_whole_user_service_config(path) {
        *base = overlay;
        return;
    }

    match (base, overlay) {
        (serde_yaml::Value::Mapping(base_map), serde_yaml::Value::Mapping(overlay_map)) => {
            for (key, value) in overlay_map {
                let pushed_path = if let Some(segment) = key.as_str().map(str::to_string) {
                    path.push(segment);
                    true
                } else {
                    false
                };
                if let Some(existing) = base_map.get_mut(&key) {
                    merge_yaml_overlay(existing, value, path);
                } else {
                    base_map.insert(key, value);
                }
                if pushed_path {
                    path.pop();
                }
            }
        }
        (base, overlay) => *base = overlay,
    }
}

fn is_replace_whole_user_service_config(path: &[String]) -> bool {
    path.len() == 3 && path[0] == "services" && path[2] == "config"
}

fn take_manifest_extras(
    yaml: &mut serde_yaml::Value,
    robot_path: &Path,
) -> Result<(RobotManifestExtras, bool)> {
    let mut extras = RobotManifestExtras::default();
    let mut stripped_extras = false;

    if let Some(root) = yaml.as_mapping_mut()
        && let Some(artifacts) = root.get_mut("artifacts")
        && let Some(artifacts) = artifacts.as_mapping_mut()
        && let Some(catalog) = artifacts.remove("catalog")
    {
        extras.catalog_source = Some(parse_catalog_source_extra(&catalog, robot_path)?);
        stripped_extras = true;
    }

    let Some(services) = yaml
        .as_mapping_mut()
        .and_then(|mapping| mapping.get_mut("services"))
        .and_then(serde_yaml::Value::as_mapping_mut)
    else {
        return Ok((extras, stripped_extras));
    };

    for (name, service) in services {
        let Some(name) = name.as_str() else {
            continue;
        };
        let Some(service) = service.as_mapping_mut() else {
            continue;
        };
        let config = service.remove("config");
        stripped_extras |= config.is_some();
        let config = config
            .map(|config| {
                serde_json::to_value(config).with_context(|| {
                    format!("services.{name}.config must be representable as JSON")
                })
            })
            .transpose()?;

        if config.is_some() {
            extras
                .user_runtimes
                .insert(name.to_string(), UserRuntimeManifestExtras { config });
        }
    }

    Ok((extras, stripped_extras))
}

fn parse_catalog_source_extra(value: &serde_yaml::Value, robot_path: &Path) -> Result<PathBuf> {
    let Some(source) = value.as_str() else {
        bail!(
            "artifacts.catalog in {} must be a local path string",
            robot_path.display()
        );
    };
    if source.trim().is_empty() {
        bail!(
            "artifacts.catalog in {} must not be empty",
            robot_path.display()
        );
    }
    Ok(PathBuf::from(source))
}

pub fn resolve(
    robot: &Robot,
    project_root: &Path,
    catalog: Option<&Catalog>,
    options: ResolveOptions,
) -> Result<ResolvedRobot> {
    if let Some(generation) = robot.artifacts.generation.as_deref() {
        bail!(
            "robot.yaml sets artifacts.generation = '{generation}', which no longer exists: \
             contract compatibility is per-contract name identity now (D1), not a single \
             per-artifact API-version ceiling. Remove artifacts.generation from robot.yaml."
        );
    }
    let channel = robot.artifacts.channel;
    let catalog_channel = selection_channel(channel);
    let target = options
        .official_target_triple
        .clone()
        .unwrap_or_else(host_target_triple);
    let platform_names = catalog
        .map(crate::catalog::service_names)
        .unwrap_or_default();
    let platform_names = platform_names
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>();
    // Finding A3: robot.yaml structural/schema validation always genuinely
    // runs (never conditionally skipped like download/build), so it always
    // gets its own truthful "validate" phase rather than the old synthetic
    // single "Preparing" phase.
    crate::session::diagnostics::run_phase(
        crate::session::event::PhaseId::new("validate"),
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
    if catalog.is_none() && !project_root.join(".phoxal/artifacts").is_dir() {
        bail!(
            "artifact catalog is unavailable and this project has no vendored binaries; run `phoxal update` online"
        );
    }
    let prefer_vendored = !options.refresh_channel_head
        && crate::host_paths::artifacts_dir().is_ok_and(|path| path.is_dir());
    let _artifact_lock = prefer_vendored
        .then(crate::native_artifacts::ArtifactStoreLock::shared)
        .transpose()?;
    if options.emit_update_notice
        && prefer_vendored
        && std::env::var_os("PHOXAL_QUIET").is_none()
        && let Some(catalog) = catalog
    {
        offer_newer_versions_notice(warn_about_newer_versions(
            robot,
            catalog,
            &target,
            &tool_target,
        ));
    }

    let mut platform_runtimes = match catalog {
        Some(catalog) => {
            resolve_catalog_entries(robot, catalog, catalog_channel, &target, prefer_vendored)?
        }
        None => resolve_vendored_entries(
            robot,
            OFFICIAL_SERVICES,
            ArtifactKind::Service,
            catalog_channel,
            &target,
        )?,
    };
    let mut simulators = match catalog {
        Some(catalog) => resolve_simulators(
            robot,
            catalog,
            catalog_channel,
            &tool_target,
            prefer_vendored,
        )?,
        None => resolve_vendored_entries(
            robot,
            OFFICIAL_SIMULATORS,
            ArtifactKind::Simulator,
            catalog_channel,
            &tool_target,
        )?,
    };

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
        catalog,
        channel: catalog_channel,
        target: &target,
        resolve_source_commits: options.resolve_source_commits,
        resolve_component_asset_commits: options.resolve_component_asset_commits,
        prefer_vendored,
        output_mode: options.output_mode,
    })?;
    let mut tools = resolve_tools(
        robot,
        catalog,
        catalog_channel,
        &tool_target,
        prefer_vendored,
    )?;
    tools.extend(resolve_native_site_artifacts(
        robot,
        catalog,
        catalog_channel,
        &tool_target,
        prefer_vendored,
        OFFICIAL_INFRASTRUCTURE,
        ArtifactKind::Infrastructure,
    )?);
    let path_overrides = apply_path_pins(
        &PathPinContext {
            robot,
            project_root,
            resolve_source_commits: options.resolve_source_commits,
            output_mode: options.output_mode,
        },
        &mut platform_runtimes,
        &mut simulators,
        &mut components,
        &mut tools,
    )?;

    Ok(ResolvedRobot {
        robot: robot.clone(),
        channel: catalog_channel,
        target,
        catalog_snapshot: catalog.map(|catalog| catalog.build.tag.clone()),
        platform_runtimes,
        simulators,
        user_runtimes,
        components,
        tools,
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
    output_mode: crate::output_mode::OutputMode,
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
                let Some(path) = resolve_git_artifact_pin_path(
                    pin,
                    context.resolve_source_commits,
                    context.output_mode,
                )?
                else {
                    continue;
                };
                path
            }
            ArtifactPin::Sha256(_) | ArtifactPin::Version(_) => continue,
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
    mode: crate::output_mode::OutputMode,
) -> Result<Option<PathBuf>> {
    if !resolve_source_commits {
        return Ok(None);
    }
    let commit = resolve_component_commit(&pin.git, &pin.rev, mode)?;
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
    tool.sha256 = crate::utils::hash_tree(path).unwrap_or_default();
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

/// Once a path override replaces a catalog-resolved runtime, its catalog
/// metadata is moot: the participant's contracts/config come from building
/// its source instead (`commands::check::source_participants_from_resolved`).
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

fn resolve_catalog_entries(
    robot: &Robot,
    catalog: &Catalog,
    channel: SelectionChannel,
    target: &str,
    prefer_vendored: bool,
) -> Result<Vec<ResolvedPlatformRuntime>> {
    OFFICIAL_SERVICES
        .iter()
        .map(|(name, package)| {
            resolved_runtime_from_expected_package(
                robot,
                catalog,
                ExpectedArtifact {
                    kind: ArtifactKind::Service,
                    name,
                    package,
                    channel,
                    target: Some(target),
                    pin_target: target,
                    assets: false,
                    prefer_vendored,
                },
            )
        })
        .collect()
}

fn resolve_simulators(
    robot: &Robot,
    catalog: &Catalog,
    channel: SelectionChannel,
    target: &str,
    prefer_vendored: bool,
) -> Result<Vec<ResolvedPlatformRuntime>> {
    OFFICIAL_SIMULATORS
        .iter()
        .map(|(name, package)| {
            resolved_runtime_from_expected_package(
                robot,
                catalog,
                ExpectedArtifact {
                    kind: ArtifactKind::Simulator,
                    name,
                    package,
                    channel,
                    target: Some(target),
                    pin_target: target,
                    assets: false,
                    prefer_vendored,
                },
            )
        })
        .collect()
}

fn resolve_vendored_entries(
    robot: &Robot,
    expected: &[(&str, &str)],
    kind: ArtifactKind,
    channel: SelectionChannel,
    target: &str,
) -> Result<Vec<ResolvedPlatformRuntime>> {
    expected
        .iter()
        .map(|(name, package)| {
            if matches!(
                robot.artifacts.pins.get(*package),
                Some(ArtifactPin::Path(_) | ArtifactPin::Git(_))
            ) {
                return Ok(ResolvedPlatformRuntime {
                    name: (*name).to_string(),
                    package: (*package).to_string(),
                    kind,
                    version: "source".to_string(),
                    artifact_ref: format!("source:{package}"),
                    sha256: None,
                    url: None,
                    size: None,
                    published: true,
                    published_triples: Vec::new(),
                    path_override: None,
                    channel,
                    target: Some(target.to_string()),
                });
            }
            vendored_runtime(name, package, kind, channel, Some(target))
        })
        .collect()
}

fn vendored_runtime(
    name: &str,
    package: &str,
    kind: ArtifactKind,
    channel: SelectionChannel,
    target: Option<&str>,
) -> Result<ResolvedPlatformRuntime> {
    let version = crate::native_artifacts::active_version_for(package)?.with_context(|| {
        format!(
            "catalog unreachable and vendored package {package} has no active version; run `phoxal update` online"
        )
    })?;
    let scope = match target {
        Some(target) => crate::native_artifacts::artifact_target_dir_for(package, target)?,
        None => crate::native_artifacts::artifact_assets_dir_for(package)?,
    };
    anyhow::ensure!(
        scope.is_dir(),
        "catalog unreachable and vendored package {package} active version {version} has no {}; run `phoxal update` online",
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
        channel,
        target: target.map(str::to_string),
    })
}

fn warn_about_newer_versions(
    robot: &Robot,
    catalog: &Catalog,
    target: &str,
    tool_target: &str,
) -> Vec<String> {
    let mut newer = Vec::new();
    for (_, package) in OFFICIAL_SERVICES {
        collect_newer(robot, catalog, package, target, &mut newer);
    }
    for (_, package) in OFFICIAL_SIMULATORS {
        collect_newer(robot, catalog, package, tool_target, &mut newer);
    }
    for (_, package) in OFFICIAL_TOOLS {
        collect_newer(robot, catalog, package, tool_target, &mut newer);
    }
    for (_, package) in OFFICIAL_INFRASTRUCTURE {
        collect_newer(robot, catalog, package, tool_target, &mut newer);
    }
    for component in robot.robot.components.values() {
        let package = format!("phoxal/component-{}", component.component);
        collect_newer(robot, catalog, &package, target, &mut newer);
    }
    newer.sort();
    newer.dedup();
    newer
}

fn offer_newer_versions_notice(newer: Vec<String>) {
    if newer.is_empty() {
        return;
    }
    crate::update_notice::offer(crate::update_notice::UpdateNotice::Artifacts(newer));
}

fn collect_newer(
    robot: &Robot,
    catalog: &Catalog,
    package: &str,
    target: &str,
    newer: &mut Vec<String>,
) {
    if robot.artifacts.pins.contains_key(package) {
        return;
    }
    let Ok(selected) = select_artifact(catalog, package, None, target) else {
        return;
    };
    let active = crate::native_artifacts::active_version_for(package)
        .ok()
        .flatten();
    if active
        .as_deref()
        .is_some_and(|version| version != selected.version)
    {
        newer.push(format!(
            "{package} {} -> {}",
            active.as_deref().unwrap_or("missing"),
            selected.version
        ));
    }
}

struct ExpectedArtifact<'a> {
    kind: ArtifactKind,
    name: &'a str,
    package: &'a str,
    channel: SelectionChannel,
    target: Option<&'a str>,
    pin_target: &'a str,
    assets: bool,
    prefer_vendored: bool,
}

fn resolved_runtime_from_expected_package(
    robot: &Robot,
    catalog: &Catalog,
    expected: ExpectedArtifact<'_>,
) -> Result<ResolvedPlatformRuntime> {
    let ExpectedArtifact {
        kind,
        name,
        package,
        channel,
        target,
        pin_target,
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
            channel,
            target: target.map(str::to_string),
        });
    }
    if prefer_vendored
        && !robot.artifacts.pins.contains_key(package)
        && let Ok(runtime) = vendored_runtime(name, package, kind, channel, target)
    {
        return Ok(runtime);
    }
    let entry = select_artifact(
        catalog,
        package,
        robot.artifacts.pins.get(package),
        pin_target,
    )?;
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
                entry.version,
                target.unwrap_or("assets")
            )
        },
        |blob| blob.url.clone(),
    );
    Ok(ResolvedPlatformRuntime {
        name: name.to_string(),
        package: package.to_string(),
        kind,
        version: entry.version.clone(),
        artifact_ref,
        sha256: built.map(|blob| blob.sha256.clone()),
        url: built.map(|blob| blob.url.clone()),
        size: built.map(|blob| blob.size),
        published: built.is_some(),
        published_triples: entry.targets.keys().cloned().collect(),
        path_override: None,
        channel,
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
fn resolve_component_commit(
    url: &str,
    git_ref: &str,
    mode: crate::output_mode::OutputMode,
) -> Result<String> {
    if is_full_commit_sha(git_ref) {
        return Ok(git_ref.to_string());
    }
    resolve_git_ref(url, git_ref, mode).with_context(|| {
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

pub fn resolve_git_ref(
    url: &str,
    git_ref: &str,
    mode: crate::output_mode::OutputMode,
) -> Result<String> {
    let progress =
        crate::progress::spinner(format!("resolving git ref {git_ref} from {url}"), mode);
    let result = resolve_git_ref_inner(url, git_ref);
    match &result {
        Ok(_) => progress.finish_and_clear(),
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

pub fn host_target_triple() -> String {
    std::env::var("PHOXAL_HOST_TARGET_TRIPLE").unwrap_or_else(|_| {
        let arch = std::env::consts::ARCH;
        let os = match std::env::consts::OS {
            "macos" => "apple-darwin",
            "linux" => "unknown-linux-gnu",
            "windows" => "pc-windows-msvc",
            other => other,
        };
        format!("{arch}-{os}")
    })
}

/// Resolve a user-supplied `--target` selector to the full catalog target
/// triple official artifacts are cataloged under. Accepts the short arch aliases
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
/// catalog. `resolve_source_commits` gates live `git ls-remote` the same way
/// it always has: metadata-only flows leave it off and skip this entirely.
/// Shared resolution context for [`resolve_component_package`]: the pieces
/// every component package slot (assets or driver) needs, bundled so the
/// per-slot resolver stays under clippy's argument-count lint.
struct ComponentResolveContext<'a> {
    robot: &'a Robot,
    catalog: Option<&'a Catalog>,
    channel: SelectionChannel,
    target: &'a str,
    resolve_source_commits: bool,
    resolve_component_asset_commits: bool,
    prefer_vendored: bool,
    output_mode: crate::output_mode::OutputMode,
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
/// precedence (`Git`/`Path`/version/sha256 pin form), otherwise it resolves
/// from the catalog. A catalog resolution also captures the matched entry's
/// built artifact for the needed scope (assets or the
/// resolved target triple for drivers) into `catalog_runtime`, exactly like a
/// service/simulator captures `artifact_ref`/`sha256`/`published` - see
/// [`resolved_runtime_from_artifact_entry`]. If the entry exists but has no
/// built artifact for that scope yet (a metadata-only entry, or not yet
/// published for this target), resolution still succeeds (the entry is real
/// and versioned - a bare `check` on an older version must not hard-fail
/// here), but `catalog_runtime` carries `sha256: None, published: false` so a
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
        return resolve_pinned_component_package(
            package,
            kind,
            pin,
            resolve_git_ref,
            context.output_mode,
        );
    }

    let (target, assets) = if kind == ArtifactKind::ComponentAssets {
        (None, true)
    } else {
        (Some(context.target), false)
    };
    let component_name = package.strip_prefix("phoxal/component-").unwrap_or(package);
    let catalog_runtime = match context.catalog {
        Some(catalog) => resolved_runtime_from_expected_package(
            context.robot,
            catalog,
            ExpectedArtifact {
                kind,
                name: component_name,
                package,
                channel: context.channel,
                target,
                pin_target: context.target,
                assets,
                prefer_vendored: context.prefer_vendored,
            },
        )
        .with_context(|| format!("failed to resolve catalog entry for {package}"))?,
        None => vendored_runtime(component_name, package, kind, context.channel, target)?,
    };

    Ok(ResolvedComponentPackage {
        package: package.to_string(),
        kind,
        source: ResolvedComponentSource::Catalog,
        path_override: None,
        catalog_runtime: Some(catalog_runtime),
    })
}

fn resolve_pinned_component_package(
    package: &str,
    kind: ArtifactKind,
    pin: &ArtifactPin,
    resolve_git_ref: bool,
    mode: crate::output_mode::OutputMode,
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
                resolve_component_commit(&pin.git, &pin.rev, mode)?
            } else {
                String::new()
            };
            ResolvedComponentSource::Git {
                git: pin.git.clone(),
                rev: commit,
                directory: pin.directory.clone(),
            }
        }
        ArtifactPin::Sha256(_) | ArtifactPin::Version(_) => {
            bail!("internal error: catalog component pin entered source resolution")
        }
    };
    Ok(ResolvedComponentPackage {
        package: package.to_string(),
        kind,
        source,
        path_override: None,
        // Source pins deliberately bypass the catalog and carry no catalog
        // runtime. Version and digest pins never enter this path.
        catalog_runtime: None,
    })
}

fn resolve_tools(
    robot: &Robot,
    catalog: Option<&Catalog>,
    channel: SelectionChannel,
    target: &str,
    prefer_vendored: bool,
) -> Result<Vec<ResolvedTool>> {
    resolve_native_site_artifacts(
        robot,
        catalog,
        channel,
        target,
        prefer_vendored,
        OFFICIAL_TOOLS,
        ArtifactKind::Tool,
    )
}

#[allow(clippy::too_many_arguments)]
fn resolve_native_site_artifacts(
    robot: &Robot,
    catalog: Option<&Catalog>,
    channel: SelectionChannel,
    target: &str,
    prefer_vendored: bool,
    artifacts: &[(&str, &str)],
    kind: ArtifactKind,
) -> Result<Vec<ResolvedTool>> {
    let Some(catalog) = catalog else {
        return artifacts
            .iter()
            .map(|(name, package)| {
                let runtime = vendored_runtime(name, package, kind, channel, Some(target))?;
                Ok(ResolvedTool {
                    kind,
                    name: format!("{}-{name}", kind.catalog_kind()),
                    package: (*package).to_string(),
                    requested: runtime.version.clone(),
                    resolved: runtime.version,
                    repo: "vendored".to_string(),
                    asset: runtime.artifact_ref,
                    binary_name: official_binary_name(kind, name),
                    sha256: String::new(),
                    url: None,
                    size: None,
                    published: true,
                    path_override: None,
                    channel,
                    target: target.to_string(),
                })
            })
            .collect();
    };
    artifacts
        .iter()
        .map(|(artifact_name, package)| {
            if matches!(
                robot.artifacts.pins.get(*package),
                Some(ArtifactPin::Path(_) | ArtifactPin::Git(_))
            ) {
                return Ok(ResolvedTool {
                    kind,
                    name: format!("{}-{artifact_name}", kind.catalog_kind()),
                    package: (*package).to_string(),
                    requested: "source".to_string(),
                    resolved: "source".to_string(),
                    repo: "source".to_string(),
                    asset: format!("source:{package}"),
                    binary_name: official_binary_name(kind, artifact_name),
                    sha256: String::new(),
                    url: None,
                    size: None,
                    published: true,
                    path_override: None,
                    channel,
                    target: target.to_string(),
                });
            }
            if prefer_vendored
                && !robot.artifacts.pins.contains_key(*package)
                && let Ok(runtime) =
                    vendored_runtime(artifact_name, package, kind, channel, Some(target))
            {
                return Ok(ResolvedTool {
                    kind,
                    name: format!("{}-{artifact_name}", kind.catalog_kind()),
                    package: (*package).to_string(),
                    requested: runtime.version.clone(),
                    resolved: runtime.version,
                    repo: "vendored".to_string(),
                    asset: runtime.artifact_ref,
                    binary_name: official_binary_name(kind, artifact_name),
                    sha256: String::new(),
                    url: None,
                    size: None,
                    published: true,
                    path_override: None,
                    channel,
                    target: target.to_string(),
                });
            }
            let entry =
                select_artifact(catalog, package, robot.artifacts.pins.get(*package), target)?;
            let built = entry.targets.get(target);
            let asset = built.map_or_else(
                || format!("{}:{}-{target}", entry.package, entry.version),
                |blob| blob.url.clone(),
            );
            Ok(ResolvedTool {
                kind,
                name: format!("{}-{artifact_name}", kind.catalog_kind()),
                package: entry.package.clone(),
                requested: entry.version.clone(),
                resolved: entry.version.clone(),
                repo: "phoxal/framework".to_string(),
                asset,
                binary_name: official_binary_name(kind, artifact_name),
                sha256: built
                    .map(|blob| blob.sha256.clone())
                    .unwrap_or_else(|| "0".repeat(64)),
                url: built.map(|blob| blob.url.clone()),
                size: built.map(|blob| blob.size),
                published: built.is_some(),
                path_override: None,
                channel,
                target: target.to_string(),
            })
        })
        .collect()
}

pub(crate) fn tool_emit_apis_id(tool_name: &str) -> &str {
    tool_name
        .strip_prefix(&format!("{PHOXAL_PROVIDER}/tool-"))
        .or_else(|| tool_name.strip_prefix(&format!("{PHOXAL_PROVIDER}/infrastructure-")))
        .or_else(|| tool_name.strip_prefix("tool-"))
        .or_else(|| tool_name.strip_prefix("infrastructure-"))
        .unwrap_or(tool_name)
}

#[cfg(test)]
mod identity_tests {
    use super::{official_binary_name, tool_emit_apis_id};
    use crate::catalog::ArtifactKind;

    #[test]
    fn tool_emit_apis_id_strips_provider_and_site_tool_prefixes() {
        assert_eq!(tool_emit_apis_id("phoxal/tool-router"), "router");
        assert_eq!(tool_emit_apis_id("tool-router"), "router");
        assert_eq!(tool_emit_apis_id("router"), "router");
        assert_eq!(tool_emit_apis_id("phoxal/infrastructure-router"), "router");
        assert_eq!(tool_emit_apis_id("infrastructure-router"), "router");
    }

    #[test]
    fn official_binary_name_uses_component_crate_binary_for_component_driver() {
        // The framework packages the component crate's own binary, named
        // `phoxal-component-<id>` (NOT `phoxal-component-<id>-driver` and NOT
        // `phoxal-component_driver-<id>`); that file is what we read.
        assert_eq!(
            official_binary_name(ArtifactKind::ComponentDriver, "ddsm115"),
            "phoxal-component-ddsm115"
        );
    }

    #[test]
    fn official_binary_name_uses_catalog_kind_for_other_kinds() {
        assert_eq!(
            official_binary_name(ArtifactKind::Service, "drive"),
            "phoxal-service-drive"
        );
        assert_eq!(
            official_binary_name(ArtifactKind::Tool, "router"),
            "phoxal-tool-router"
        );
        assert_eq!(
            official_binary_name(ArtifactKind::Simulator, "webots-supervisor"),
            "phoxal-simulator-webots-supervisor"
        );
    }
}

/// The official binary name a resolved artifact's packaged tarball contains.
/// For most kinds this is `phoxal-<catalog_kind>-<name>`
/// (`phoxal-service-drive`, `phoxal-tool-router`,
/// `phoxal-simulator-webots-supervisor`). A `ComponentDriver`'s binary is
/// `phoxal-component-<id>` (its catalog `kind` tag `component_driver` is not
/// part of the binary name): the framework packages the component crate's own
/// binary, named `phoxal-component-<id>`, and that is the file we read.
/// `ComponentAssets` has no runtime binary at all and must never reach this
/// function.
pub(crate) fn official_binary_name(kind: ArtifactKind, name: &str) -> String {
    match kind {
        ArtifactKind::ComponentDriver => format!("phoxal-component-{name}"),
        ArtifactKind::ComponentAssets => {
            unreachable!("component_assets has no runtime binary to name")
        }
        ArtifactKind::Service
        | ArtifactKind::Tool
        | ArtifactKind::Simulator
        | ArtifactKind::Infrastructure => {
            format!("phoxal-{kind}-{name}")
        }
    }
}

/// The filesystem/tag-safe projection of a provider-qualified package id
/// (`phoxal/service-drive` -> `phoxal-service-drive`), used for the synthetic
/// `artifact_ref` fallback when a catalog entry has no built artifact yet for
/// the resolved target (docs #21's release-tag/asset projection).
fn filesystem_safe_package_name(package: &str) -> String {
    package.replace('/', "-")
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
    use crate::catalog::{
        SelectionChannel as CatalogChannel, fixture_catalog_for_tests,
        fixture_component_assets_entry_for_tests, fixture_contract_for_tests,
        fixture_service_entry_for_tests,
    };
    use crate::host_paths::test_support::ScratchPhoxalHome;

    fn test_catalog() -> Catalog {
        fixture_catalog_for_tests(vec![
            fixture_service_entry_for_tests(
                "drive",
                "0.1.0",
                CatalogChannel::Stable,
                &host_target_triple(),
                // Published so the package resolves for this host target
                // without robot.yaml needing any pin at all (D1: no
                // `artifacts.generation` ceiling to auto-detect anymore).
                true,
                vec![fixture_contract_for_tests("v1::drive::Target", "publish")],
            ),
            fixture_component_assets_entry_for_tests("ddsm115", "0.1.0", CatalogChannel::Stable),
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
        let catalog = test_catalog();
        let resolved = resolve(
            &robot,
            std::path::Path::new("."),
            Some(&catalog),
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
  channel: stable
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
        let catalog = test_catalog();
        let resolved = resolve(
            &robot,
            std::path::Path::new("."),
            Some(&catalog),
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
        // no official `phoxal/component-<id>` assets package in the catalog
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
artifacts:
  channel: stable
"#,
        )?;
        // `test_catalog()` carries no `phoxal/component-passive_caster`
        // entry at all - exactly the "absent from snapshot" case the bug
        // report hit.
        let catalog = test_catalog();
        let resolved = resolve(
            &robot,
            std::path::Path::new("."),
            Some(&catalog),
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
artifacts:
  channel: stable
"#,
        )?;
        let catalog = test_catalog();
        let error = resolve(
            &robot,
            std::path::Path::new("."),
            Some(&catalog),
            ResolveOptions {
                resolve_source_commits: false,
                resolve_component_asset_commits: false,
                ..ResolveOptions::default()
            },
        )
        .expect_err("a driven component with no catalog entry at all must hard-fail");

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
                "expected package phoxal/component-unknown_driven_component is absent from \
                 snapshot"
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
            channel: CatalogChannel::Stable,
            target: Some("aarch64-unknown-linux-gnu".to_string()),
        };

        assert_eq!(runtime.artifact_ref(), "service-asset:v1-stable");
    }

    #[test]
    fn in_project_path_pins_allowed_in_base_but_escaping_ones_rejected() {
        let base = |pin: &str| {
            serde_yaml::from_str::<serde_yaml::Value>(&format!(
                "artifacts:\n  pins:\n    phoxal/component-local:\n      path: {pin}\n"
            ))
            .unwrap()
        };
        let manifest = Path::new("/proj/robot.yaml");

        // In-project component paths are permanent robot content: allowed in base.
        for ok in ["./components/passive_caster", "components/x", "a/b/../c"] {
            assert!(
                ensure_no_base_path_pins(&base(ok), manifest).is_ok(),
                "in-project pin {ok} should be allowed in the base manifest"
            );
        }
        // Escaping / absolute paths are dev overrides: overlay only.
        for bad in ["../framework/service/drive", "/abs/path", "../../x"] {
            assert!(
                ensure_no_base_path_pins(&base(bad), manifest).is_err(),
                "escaping pin {bad} must be rejected in the base manifest"
            );
        }
    }

    #[test]
    fn path_pin_escape_detection_is_lexical() {
        let project_root = Path::new("/proj");
        assert!(!path_pin_escapes_project(
            Path::new("./components/x"),
            project_root
        ));
        assert!(!path_pin_escapes_project(
            Path::new("a/b/../c"),
            project_root
        ));
        assert!(path_pin_escapes_project(Path::new("../x"), project_root));
        assert!(path_pin_escapes_project(
            Path::new("a/../../x"),
            project_root
        ));
        assert!(path_pin_escapes_project(Path::new("/abs"), project_root));
    }

    #[cfg(unix)]
    #[test]
    fn base_path_pin_rejects_symlink_escape_and_allows_real_project_dir() -> anyhow::Result<()> {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir()?;
        let project_root = temp.path().join("robot");
        let components = project_root.join("components");
        let local = components.join("local");
        let outside = temp.path().join("outside");
        fs::create_dir_all(&local)?;
        fs::create_dir_all(&outside)?;
        symlink(&outside, components.join("escaped"))?;
        let manifest = project_root.join("robot.yaml");

        let base = |pin: &str| {
            serde_yaml::from_str::<serde_yaml::Value>(&format!(
                "artifacts:\n  pins:\n    phoxal/component-local:\n      path: {pin}\n"
            ))
            .expect("test manifest should parse")
        };

        assert!(ensure_no_base_path_pins(&base("components/local"), &manifest).is_ok());
        let error = ensure_no_base_path_pins(&base("components/escaped"), &manifest)
            .expect_err("symlink outside the project must be rejected");
        assert!(error.to_string().contains("dev-overlay only"), "{error:#}");
        Ok(())
    }
}
