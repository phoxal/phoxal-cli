use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use phoxal::model::robot::{
    RobotV1 as Robot,
    v1::{ArtifactPin, Channel, UserService},
};
use serde_json::Value;

use crate::catalog::{
    ArtifactKind, ArtifactStatus, CatalogEntry, CatalogRevision, Channel as CatalogChannel,
    ContractUse, ReleaseAssetMetadata, compare_generations,
};
use crate::shell;
use crate::utils::{hash_tree, resolve_project_path};

/// The provider every official Phoxal package uses in its provider-qualified
/// `artifacts.pins` / catalog `package` identity (`phoxal/service-drive`, ...).
const PHOXAL_PROVIDER: &str = "phoxal";

const ROBOT_FILE: &str = "robot.yaml";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolveOptions {
    /// Resolve git component `tag` → `commit`. A `tag` that is already a full
    /// commit SHA resolves with no network; a tag/branch ref is resolved live
    /// via `git ls-remote`. Flows that need to locate/stage component driver
    /// sources (`check`, `run --watch`, simulate, `deploy`) set this;
    /// flows that never read component commits (`pull`, `outdated`) leave it
    /// off so they stay fully offline.
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
}

impl Default for ResolveOptions {
    fn default() -> Self {
        Self {
            resolve_source_commits: true,
            resolve_component_asset_commits: true,
            official_target_triple: None,
            tool_target_triple: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedRobot {
    pub robot: Robot,
    pub target_generation: String,
    pub channel: Channel,
    pub target: String,
    pub catalog_revision: Option<String>,
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedPlatformRuntime {
    pub name: String,
    pub package: String,
    pub kind: ArtifactKind,
    pub generation: String,
    pub version: String,
    pub artifact_ref: String,
    pub sha256: Option<String>,
    pub metadata: Option<ResolvedArtifactMetadata>,
    pub target_status: Option<ArtifactStatus>,
    pub per_triple_status: BTreeMap<String, ArtifactStatus>,
    pub changed_contracts: Vec<String>,
    pub contract_uses: Vec<ContractUse>,
    pub path_override: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedArtifactMetadata {
    pub emit_apis: String,
    pub emit_apis_sha256: String,
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedComponent {
    pub instance: String,
    /// The logical component id (`component: <id>` in `robot.yaml`).
    pub source_name: String,
    /// The resolved `component_assets` package. Always present.
    pub assets: ResolvedComponentPackage,
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedComponentPackage {
    /// The provider-qualified package id (`phoxal/component-ddsm115-assets`).
    pub package: String,
    pub kind: ArtifactKind,
    pub source: ResolvedComponentSource,
    pub path_override: Option<PathBuf>,
    /// Present exactly when `source == Catalog` and the catalog resolved a
    /// matching entry with a release asset for the needed scope
    /// (target-independent for assets, `context.target` for drivers). Carries
    /// the same shape a service/simulator resolves to
    /// ([`ResolvedPlatformRuntime`]) so components stage through the identical
    /// native-artifact machinery (`native_artifacts::NativeArtifactDescriptor`)
    /// instead of a parallel bespoke path. `None` for `Path`/`Git` sources, and
    /// for a `Catalog` source whose entry has no release asset yet (a
    /// metadata-only / not-yet-published catalog entry) - callers that need to
    /// stage a real bundle must treat that as a clear "not available" error,
    /// not silently succeed.
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
    /// package); staged from a `CatalogEntry` release asset.
    Catalog,
}

/// A resolved native tool (`tool-router`, `tool-joypad`). `name` is the short,
/// launch-safe kind-qualified id used for participant/site ids, systemd unit
/// names, and env var keys (`SITE_TOOL_ROUTER` etc.); `package` is the
/// canonical provider-qualified identity (`phoxal/tool-router`) used for
/// catalog lookups and native-artifact provisioning.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedTool {
    pub name: String,
    pub package: String,
    pub requested: String,
    pub resolved: String,
    pub repo: String,
    pub asset: String,
    pub binary_name: String,
    pub sha256: String,
    pub metadata: Option<ResolvedArtifactMetadata>,
    pub path_override: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolvedPathOverrideKind {
    Service,
    ComponentAssets,
    ComponentDriver,
    Tool,
    Simulator,
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

    let path_pins = pins
        .iter()
        .filter_map(|(key, value)| {
            let path_key = serde_yaml::Value::String("path".to_string());
            let has_path = value
                .as_mapping()
                .is_some_and(|mapping| mapping.contains_key(&path_key));
            has_path.then(|| key.as_str().unwrap_or("<non-string>").to_string())
        })
        .collect::<Vec<_>>();
    if path_pins.is_empty() {
        return Ok(());
    }
    bail!(
        "{path}: artifacts.pins path overrides are dev-overlay only; move {} to robot.<env>.yaml and load it with --env <env>",
        path_pins.join(", "),
        path = path.display()
    )
}

fn parse_robot_value_with_extras(yaml: &mut serde_yaml::Value, path: &Path) -> Result<LoadedRobot> {
    let (extras, _) = take_manifest_extras(yaml, path)?;
    let sanitized = serde_yaml::to_string(&yaml)
        .with_context(|| format!("failed to prepare {}", path.display()))?;
    let robot = Robot::read_from_string(&sanitized)?;
    validate_launch_participant_ids(&robot, path)?;

    Ok(LoadedRobot { robot, extras })
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
    catalog: Option<&CatalogRevision>,
    options: ResolveOptions,
) -> Result<ResolvedRobot> {
    let channel = robot.artifacts.channel;
    let catalog_channel = CatalogChannel::from(channel);
    let target = options
        .official_target_triple
        .clone()
        .unwrap_or_else(host_target_triple);
    let target_generation = target_generation(robot, catalog, catalog_channel, &target)?;
    let platform_names = catalog
        .map(CatalogRevision::service_names)
        .unwrap_or_default();
    let platform_names = platform_names
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>();
    robot
        .validate_with(&platform_names)
        .map_err(|errors| anyhow!("Robot errors:\n{}", join_errors(errors)))?;
    let tool_target = options
        .tool_target_triple
        .unwrap_or_else(host_target_triple);

    let mut platform_runtimes = catalog
        .map(|catalog| {
            resolve_catalog_entries(catalog, catalog_channel, &target, &target_generation)
        })
        .transpose()?
        .unwrap_or_default();
    let mut simulators = catalog
        .map(|catalog| {
            resolve_simulators(catalog, catalog_channel, &tool_target, &target_generation)
        })
        .transpose()?
        .unwrap_or_default();

    let user_runtimes = robot
        .services
        .iter()
        .map(|(name, service)| resolve_user_runtime(project_root, name, service))
        .collect::<Result<Vec<_>>>()?;

    // Git ref → commit SHA resolution: when `resolve_source_commits` is set, a
    // `rev` that is already a full commit SHA resolves with no network, while a
    // tag/branch ref is resolved live via `git ls-remote`. Flows that never read
    // component commits (`pull`, `outdated`) leave it off so they stay offline.
    // Component assets have a separate gate because deploy/check/run do not
    // read non-local asset bundles during planning/staging; live simulate does.
    let mut components = resolve_components(
        robot,
        catalog,
        catalog_channel,
        &target,
        &target_generation,
        options.resolve_source_commits,
        options.resolve_component_asset_commits,
    )?;
    let mut tools = resolve_tools(
        robot,
        catalog,
        catalog_channel,
        &tool_target,
        &target_generation,
    )?;
    let path_overrides = apply_path_pins(
        robot,
        project_root,
        &mut platform_runtimes,
        &mut simulators,
        &mut components,
        &mut tools,
    )?;

    Ok(ResolvedRobot {
        robot: robot.clone(),
        target_generation,
        channel,
        target,
        catalog_revision: catalog.map(|catalog| catalog.revision.clone()),
        platform_runtimes,
        simulators,
        user_runtimes,
        components,
        tools,
        path_overrides,
    })
}

fn apply_path_pins(
    robot: &Robot,
    project_root: &Path,
    platform_runtimes: &mut [ResolvedPlatformRuntime],
    simulators: &mut [ResolvedPlatformRuntime],
    components: &mut [ResolvedComponent],
    tools: &mut [ResolvedTool],
) -> Result<Vec<ResolvedPathOverride>> {
    let mut overrides = Vec::new();
    for (key, pin) in &robot.artifacts.pins {
        let ArtifactPin::Path(pin) = pin else {
            continue;
        };
        let path = resolve_project_path(project_root, &pin.path);
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
    runtime.path_override = Some(path.to_path_buf());
    runtime.artifact_ref = format!("path:{}", path.display());
    runtime.sha256 = None;
    runtime.metadata = None;
    runtime.target_status = Some(ArtifactStatus::Released);
    runtime.changed_contracts.clear();
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
        if component.assets.package == key {
            component.assets.path_override = Some(path.to_path_buf());
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
    tool.metadata = None;
    overrides.push(ResolvedPathOverride {
        key: key.to_string(),
        kind: ResolvedPathOverrideKind::Tool,
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
    runtime.path_override = Some(path.to_path_buf());
    runtime.artifact_ref = format!("path:{}", path.display());
    runtime.sha256 = None;
    runtime.metadata = None;
    runtime.target_status = Some(ArtifactStatus::Released);
    runtime.changed_contracts.clear();
    overrides.push(ResolvedPathOverride {
        key: key.to_string(),
        kind: ResolvedPathOverrideKind::Simulator,
        artifact_name: runtime.name.clone(),
        path: path.to_path_buf(),
    });
    true
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
    phoxal::model::robot::v1::is_provider_qualified_pin_key(key)
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
            .map(|component| component.assets.package.clone()),
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

fn target_generation(
    robot: &Robot,
    catalog: Option<&CatalogRevision>,
    channel: CatalogChannel,
    target: &str,
) -> Result<String> {
    if let Some(generation) = robot.artifacts.generation.as_deref() {
        return Ok(generation.to_string());
    }
    if let Some(catalog) = catalog {
        if let Some(generation) = catalog.newest_generation_on_channel(channel, target) {
            return Ok(generation);
        }
        if robot_uses_only_catalog_artifacts(robot) {
            bail!(
                "{}",
                catalog_target_generation_not_yet_available(catalog, channel, target)
            );
        }
    }
    if robot_uses_only_catalog_artifacts(robot) {
        bail!("{}", crate::catalog::unavailable_catalog_error());
    }
    Ok("source".to_string())
}

fn robot_uses_only_catalog_artifacts(robot: &Robot) -> bool {
    robot.services.is_empty()
        && robot
            .robot
            .components
            .values()
            .all(|component| component.driver.is_none())
}

fn catalog_target_generation_not_yet_available(
    catalog: &CatalogRevision,
    channel: CatalogChannel,
    target: &str,
) -> anyhow::Error {
    anyhow!(
        "NotYetAvailable: loaded artifact catalog revision {} has no released generation assets for target {target} on channel {channel}. Pass a catalog that includes released assets for {target}/{channel}, pin artifacts.generation for source-only development, or deploy a target covered by this catalog.",
        catalog.revision
    )
}

pub(crate) fn target_generation_for_robot(
    robot: &Robot,
    catalog: Option<&CatalogRevision>,
) -> Result<String> {
    target_generation(
        robot,
        catalog,
        CatalogChannel::from(robot.artifacts.channel),
        &host_target_triple(),
    )
}

fn resolve_catalog_entries(
    catalog: &CatalogRevision,
    channel: CatalogChannel,
    target: &str,
    target_generation: &str,
) -> Result<Vec<ResolvedPlatformRuntime>> {
    let mut selected = BTreeMap::<String, &CatalogEntry>::new();
    for entry in &catalog.entries {
        if entry.kind != ArtifactKind::Service {
            continue;
        }
        if !entry.channels.contains_key(&channel) {
            continue;
        }
        if !entry.target_triples.iter().any(|triple| triple == target) {
            continue;
        }
        if compare_generations(&entry.api_generation, target_generation).is_gt() {
            continue;
        }
        selected
            .entry(entry.package.clone())
            .and_modify(|existing| {
                if compare_catalog_entries(entry, existing).is_gt() {
                    *existing = entry;
                }
            })
            .or_insert(entry);
    }

    ensure_catalog_schema_agreement(selected.values().copied())?;

    selected
        .into_values()
        .map(|entry| {
            let name = entry
                .artifact_name()
                .ok_or_else(|| anyhow!("{} does not match kind {}", entry.package, entry.kind))?
                .to_string();
            let release_asset = entry.release_assets.get(target);
            let artifact_ref = release_asset
                .map(|asset| asset.asset.clone())
                .unwrap_or_else(|| {
                    format!(
                        "{}:{}-{}-{}-{}",
                        filesystem_safe_package_name(&entry.package),
                        entry.version,
                        entry.api_generation,
                        channel,
                        target
                    )
                });
            Ok(ResolvedPlatformRuntime {
                name,
                package: entry.package.clone(),
                kind: entry.kind,
                generation: entry.api_generation.clone(),
                version: entry.version.clone(),
                artifact_ref,
                sha256: release_asset.map(|asset| asset.sha256.clone()),
                metadata: release_asset
                    .and_then(|asset| resolved_metadata_opt(asset.metadata.as_ref())),
                target_status: entry.status_for(target),
                per_triple_status: entry.status.clone(),
                changed_contracts: entry.changed_contracts.clone(),
                contract_uses: entry.contract_uses.clone(),
                path_override: None,
            })
        })
        .collect()
}

fn resolve_simulators(
    catalog: &CatalogRevision,
    channel: CatalogChannel,
    target: &str,
    target_generation: &str,
) -> Result<Vec<ResolvedPlatformRuntime>> {
    let selected = select_latest_entries(
        catalog,
        ArtifactKind::Simulator,
        channel,
        target,
        target_generation,
    );
    selected
        .into_values()
        .map(|entry| resolved_runtime_from_entry(entry, target))
        .collect()
}

fn select_latest_entries<'a>(
    catalog: &'a CatalogRevision,
    kind: ArtifactKind,
    channel: CatalogChannel,
    target: &str,
    target_generation: &str,
) -> BTreeMap<String, &'a CatalogEntry> {
    let mut selected = BTreeMap::<String, &CatalogEntry>::new();
    for entry in &catalog.entries {
        if entry.kind != kind {
            continue;
        }
        if !entry.channels.contains_key(&channel) {
            continue;
        }
        if !entry.target_triples.iter().any(|triple| triple == target) {
            continue;
        }
        if compare_generations(&entry.api_generation, target_generation).is_gt() {
            continue;
        }
        selected
            .entry(entry.package.clone())
            .and_modify(|existing| {
                if compare_catalog_entries(entry, existing).is_gt() {
                    *existing = entry;
                }
            })
            .or_insert(entry);
    }
    selected
}

pub(crate) fn resolved_runtime_from_entry(
    entry: &CatalogEntry,
    target: &str,
) -> Result<ResolvedPlatformRuntime> {
    let name = entry
        .artifact_name()
        .ok_or_else(|| anyhow!("{} does not match kind {}", entry.package, entry.kind))?
        .to_string();
    let release_asset = entry.release_assets.get(target);
    let artifact_ref = release_asset
        .map(|asset| asset.asset.clone())
        .unwrap_or_else(|| {
            format!(
                "{}:{}-{}-{}",
                filesystem_safe_package_name(&entry.package),
                entry.version,
                entry.api_generation,
                target
            )
        });
    Ok(ResolvedPlatformRuntime {
        name,
        package: entry.package.clone(),
        kind: entry.kind,
        generation: entry.api_generation.clone(),
        version: entry.version.clone(),
        artifact_ref,
        sha256: release_asset.map(|asset| asset.sha256.clone()),
        metadata: release_asset.and_then(|asset| resolved_metadata_opt(asset.metadata.as_ref())),
        target_status: entry.status_for(target),
        per_triple_status: entry.status.clone(),
        changed_contracts: entry.changed_contracts.clone(),
        contract_uses: entry.contract_uses.clone(),
        path_override: None,
    })
}

fn resolved_metadata(metadata: &ReleaseAssetMetadata) -> ResolvedArtifactMetadata {
    ResolvedArtifactMetadata {
        emit_apis: metadata.emit_apis.clone(),
        emit_apis_sha256: metadata.emit_apis_sha256.clone(),
    }
}

/// `component_assets` release assets carry no `metadata` (docs #21: a
/// target-independent asset bundle has no runtime binary to describe); every
/// other kind's release asset always carries `Some`. Callers building a
/// [`ResolvedPlatformRuntime`]/[`ResolvedComponentPackage`] from a release
/// asset route through this so a missing sidecar becomes `None` metadata
/// rather than a panic.
fn resolved_metadata_opt(
    metadata: Option<&ReleaseAssetMetadata>,
) -> Option<ResolvedArtifactMetadata> {
    metadata.map(resolved_metadata)
}

fn compare_catalog_entries(left: &CatalogEntry, right: &CatalogEntry) -> std::cmp::Ordering {
    compare_generations(&left.api_generation, &right.api_generation).then_with(|| {
        match (
            semver::Version::parse(&left.version),
            semver::Version::parse(&right.version),
        ) {
            (Ok(left), Ok(right)) => left.cmp(&right),
            _ => left.version.cmp(&right.version),
        }
    })
}

fn ensure_catalog_schema_agreement<'a>(
    entries: impl IntoIterator<Item = &'a CatalogEntry>,
) -> Result<()> {
    let mut schemas = BTreeMap::<(&str, &str), BTreeMap<&str, Vec<&str>>>::new();
    for entry in entries {
        for contract in &entry.contract_uses {
            schemas
                .entry((&contract.family, &contract.topic_template))
                .or_default()
                .entry(&contract.schema_id)
                .or_default()
                .push(&entry.package);
        }
    }
    for ((family, topic), schema_ids) in schemas {
        if schema_ids.len() > 1 {
            let reporters = schema_ids
                .into_iter()
                .map(|(schema_id, artifacts)| format!("{schema_id} ({})", artifacts.join(", ")))
                .collect::<Vec<_>>()
                .join("; ");
            bail!(
                "artifact catalog cannot resolve one schema_id for {family} ({topic}): {reporters}"
            );
        }
    }
    Ok(())
}

/// Resolve a git component ref to a concrete commit SHA.
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
            "could not resolve git component ref '{git_ref}' from {url} without network access. \
             Pin the component to an explicit commit SHA in robot.yaml (artifacts.pins.<package>.rev: <40-char sha>), \
             or run with network access so `git ls-remote` can resolve the ref."
        )
    })
}

fn is_full_commit_sha(value: &str) -> bool {
    value.len() == 40 && value.chars().all(|byte| byte.is_ascii_hexdigit())
}

pub fn resolve_git_ref(url: &str, git_ref: &str) -> Result<String> {
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

/// Resolve every `robot.components.<instance>` entry: `component: <id>`
/// always resolves `phoxal/component-<id>-assets`, and resolves
/// `phoxal/component-<id>-driver` only when the instance declares a `driver`
/// block. A driverless instance is valid and stages no driver package. An
/// instance that declares `driver` but has no matching driver package in the
/// resolved graph is a hard [`ComponentDriverUnavailable`] error.
///
/// Forks may replace either package slot via `artifacts.pins`; a pin with a
/// `Git`/`Path` form resolves that package from the fork instead of the
/// catalog. `resolve_source_commits` gates live `git ls-remote` the same way
/// it always has: `pull`/`outdated` leave it off to stay fully offline.
/// Shared resolution context for [`resolve_component_package`]: the pieces
/// every component package slot (assets or driver) needs, bundled so the
/// per-slot resolver stays under clippy's argument-count lint.
struct ComponentResolveContext<'a> {
    robot: &'a Robot,
    catalog: Option<&'a CatalogRevision>,
    channel: CatalogChannel,
    target: &'a str,
    target_generation: &'a str,
    resolve_source_commits: bool,
    resolve_component_asset_commits: bool,
}

fn resolve_components(
    robot: &Robot,
    catalog: Option<&CatalogRevision>,
    channel: CatalogChannel,
    target: &str,
    target_generation: &str,
    resolve_source_commits: bool,
    resolve_component_asset_commits: bool,
) -> Result<Vec<ResolvedComponent>> {
    let context = ComponentResolveContext {
        robot,
        catalog,
        channel,
        target,
        target_generation,
        resolve_source_commits,
        resolve_component_asset_commits,
    };
    let mut components = Vec::new();
    for (instance_name, instance) in &robot.robot.components {
        let component_id = &instance.component;
        let assets_package = format!("{PHOXAL_PROVIDER}/component-{component_id}-assets");
        let driver_package = format!("{PHOXAL_PROVIDER}/component-{component_id}-driver");

        let assets = resolve_component_package(
            &context,
            &assets_package,
            ArtifactKind::ComponentAssets,
            context.resolve_component_asset_commits,
        )
        .with_context(|| {
            format!(
                "robot.components.{instance_name}.component '{component_id}' failed to resolve its component_assets package"
            )
        })?;

        let has_driver = instance.driver.is_some();
        let driver = if has_driver {
            match resolve_component_package(
                &context,
                &driver_package,
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
/// release asset for the needed scope (target-independent for assets, the
/// resolved target triple for drivers) into `catalog_runtime`, exactly like a
/// service/simulator captures `artifact_ref`/`sha256`/`metadata` - see
/// [`resolved_runtime_from_entry`]. If the entry exists but has no release
/// asset for that scope (a metadata-only entry, or not yet published for this
/// target), resolution still succeeds (the entry is real and versioned - a
/// bare `check` on an older generation must not hard-fail here), but
/// `catalog_runtime` carries `sha256: None, metadata: None` so a later staging
/// attempt reports a clear diagnostic instead of silently succeeding with no
/// bundle to fetch.
fn resolve_component_package(
    context: &ComponentResolveContext<'_>,
    package: &str,
    kind: ArtifactKind,
    resolve_git_ref: bool,
) -> Result<ResolvedComponentPackage> {
    if let Some(pin) = context.robot.artifacts.pins.get(package) {
        return resolve_pinned_component_package(package, kind, pin, resolve_git_ref);
    }

    let Some(catalog) = context.catalog else {
        bail!("no artifact catalog revision is available to resolve {package}");
    };
    let target_scope = if kind == ArtifactKind::ComponentAssets {
        crate::catalog::TARGET_INDEPENDENT_SCOPE
    } else {
        context.target
    };
    let entry = catalog
        .entries
        .iter()
        .filter(|entry| entry.kind == kind && entry.package == package)
        .filter(|entry| entry.channels.contains_key(&context.channel))
        .filter(|entry| {
            entry
                .target_triples
                .iter()
                .any(|triple| triple == target_scope)
        })
        .filter(|entry| {
            compare_generations(&entry.api_generation, context.target_generation).is_le()
        })
        .max_by(|left, right| compare_catalog_entries(left, right))
        .ok_or_else(|| anyhow!("{package} is not available in the artifact catalog"))?;

    let catalog_runtime = resolved_runtime_from_entry(entry, target_scope)
        .with_context(|| format!("failed to resolve catalog entry for {package}"))?;

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
        ArtifactPin::Sha256(_) | ArtifactPin::Version(_) => ResolvedComponentSource::Catalog,
    };
    Ok(ResolvedComponentPackage {
        package: package.to_string(),
        kind,
        source,
        path_override: None,
        // A `Sha256`/`Version` artifact pin does not (yet) re-enter the
        // catalog to capture a release asset; unchanged pre-existing
        // behavior. `Git`/`Path` pins never have a catalog runtime.
        catalog_runtime: None,
    })
}

fn resolve_tools(
    robot: &Robot,
    catalog: Option<&CatalogRevision>,
    channel: CatalogChannel,
    target: &str,
    target_generation: &str,
) -> Result<Vec<ResolvedTool>> {
    let Some(catalog) = catalog else {
        return Ok(Vec::new());
    };
    let _ = robot;

    let tool_entries = catalog
        .entries
        .iter()
        .filter(|entry| entry.kind == ArtifactKind::Tool)
        .collect::<Vec<_>>();

    let mut selected = BTreeMap::<String, &CatalogEntry>::new();
    for entry in tool_entries {
        if !entry.target_triples.iter().any(|triple| triple == target) {
            continue;
        }
        if compare_generations(&entry.api_generation, target_generation).is_gt() {
            continue;
        }
        if !entry.channels.contains_key(&channel) {
            continue;
        }
        selected
            .entry(entry.package.clone())
            .and_modify(|existing| {
                if compare_catalog_entries(entry, existing).is_gt() {
                    *existing = entry;
                }
            })
            .or_insert(entry);
    }

    selected
        .into_values()
        .map(|entry| {
            let release_asset = entry.release_assets.get(target);
            let asset = release_asset
                .map(|asset| asset.asset.clone())
                .unwrap_or_else(|| {
                    format!(
                        "{}:{}-{}-{}",
                        entry.package, entry.version, entry.api_generation, target
                    )
                });
            let artifact_name = entry.artifact_name().unwrap_or("");
            Ok(ResolvedTool {
                name: format!("tool-{artifact_name}"),
                package: entry.package.clone(),
                requested: entry.version.clone(),
                resolved: entry.version.clone(),
                repo: "phoxal/framework".to_string(),
                asset,
                binary_name: official_binary_name(entry.kind, artifact_name),
                sha256: release_asset
                    .map(|asset| asset.sha256.clone())
                    .unwrap_or_else(|| "0".repeat(64)),
                metadata: release_asset
                    .and_then(|asset| resolved_metadata_opt(asset.metadata.as_ref())),
                path_override: None,
            })
        })
        .collect()
}

pub(crate) fn tool_emit_apis_id(tool_name: &str) -> &str {
    tool_name
        .strip_prefix(&format!("{PHOXAL_PROVIDER}/tool-"))
        .or_else(|| tool_name.strip_prefix("tool-"))
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
    }

    #[test]
    fn official_binary_name_uses_package_shape_for_component_driver() {
        // docs #21: the driver binary is `phoxal-component-<id>-driver`, NOT
        // `phoxal-component_driver-<id>` (the catalog kind tag).
        assert_eq!(
            official_binary_name(ArtifactKind::ComponentDriver, "ddsm115"),
            "phoxal-component-ddsm115-driver"
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
/// `phoxal-simulator-webots-supervisor`). A `ComponentDriver` is the one
/// exception: its catalog `kind` (`component_driver`) is not its binary name -
/// docs #21 names the driver binary `phoxal-component-<id>-driver`, matching
/// the package-id shape (`phoxal/component-<id>-driver`) rather than the
/// catalog kind tag. `ComponentAssets` has no runtime binary at all and must
/// never reach this function.
pub(crate) fn official_binary_name(kind: ArtifactKind, name: &str) -> String {
    match kind {
        ArtifactKind::ComponentDriver => format!("phoxal-component-{name}-driver"),
        ArtifactKind::ComponentAssets => {
            unreachable!("component_assets has no runtime binary to name")
        }
        ArtifactKind::Service | ArtifactKind::Tool | ArtifactKind::Simulator => {
            format!("phoxal-{kind}-{name}")
        }
    }
}

/// The filesystem/tag-safe projection of a provider-qualified package id
/// (`phoxal/service-drive` -> `phoxal-service-drive`), used for the synthetic
/// `artifact_ref` fallback when a catalog entry has no real release asset yet
/// (docs #21's release-tag/asset projection).
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
        ArtifactStatus, Channel as CatalogChannel, fixture_catalog_for_tests,
        fixture_component_assets_entry_for_tests, fixture_contract_for_tests,
        fixture_service_entry_for_tests,
    };

    fn test_catalog() -> CatalogRevision {
        fixture_catalog_for_tests(vec![
            fixture_service_entry_for_tests(
                "drive",
                "y2026_1",
                "0.1.0",
                CatalogChannel::Stable,
                &host_target_triple(),
                ArtifactStatus::Pending,
                vec![fixture_contract_for_tests(
                    "drive::Target",
                    "drive/target",
                    "publish",
                    "0123456789abcdef",
                )],
            ),
            fixture_component_assets_entry_for_tests(
                "ddsm115",
                "y2026_1",
                "0.1.0",
                CatalogChannel::Stable,
            ),
        ])
    }

    #[test]
    fn resolve_without_source_commits_leaves_git_component_commits_empty() -> anyhow::Result<()> {
        // Flows that never read component commits (`pull`, `outdated`) resolve
        // with `resolve_source_commits: false` and must NOT run `git ls-remote`.
        // A git component pin is resolved with an empty commit; if resolution
        // tried to reach the network it would either hang or fail, so an empty
        // commit proves no ls-remote was attempted.
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
        match &git_component.assets.source {
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

    const GIT_COMPONENT_ROBOT: &str = r#"schema: v0
robot:
  id: testbot
  namespace: test
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
    phoxal/component-ddsm115-assets:
      git: https://github.com/phoxal/framework
      rev: main
      directory: component/ddsm115
"#;

    #[test]
    fn explicit_commit_sha_tag_resolves_without_network() -> anyhow::Result<()> {
        // A `rev` that is already a full commit SHA is an explicit pin: it must
        // resolve with no network (no `git ls-remote`), so a live-resolution
        // flow works offline when components are pinned to a SHA.
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
        match &git_component.assets.source {
            ResolvedComponentSource::Git { rev, .. } => {
                assert_eq!(rev, sha);
            }
            other => panic!("expected a git component source, got {other:?}"),
        }
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
            r#"schema: v0
robot:
  id: bot
  namespace: dev
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
    fn load_robot_keeps_typed_bus_section() -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        let path = temp.path().join("robot.yaml");
        std::fs::write(
            &path,
            r#"schema: v0
robot:
  id: bot
  namespace: dev
  kinematic:
    kind: differential
    left_actuators: [l.motor]
    right_actuators: [r.motor]
    left_encoders: [l.encoder]
    right_encoders: [r.encoder]
    wheel_radius_m: 0.1
    wheel_base_m: 0.5
bus:
  listen:
    - tcp/127.0.0.1:7448
    - serial//dev/ttyUSB0?baudrate=115200
  uplink:
    connect: tls/cloud.phoxal.example:7447
    auth:
      ca: identity/ca.pem
      cert: identity/robot.pem
      key: identity/robot.key
    retry:
      initial_ms: 2000
      max_ms: 10000
"#,
        )?;

        let loaded = load_robot_with_extras(&path)?;
        assert_eq!(
            loaded.robot.bus.listen,
            vec![
                "tcp/127.0.0.1:7448".to_string(),
                "serial//dev/ttyUSB0?baudrate=115200".to_string(),
            ]
        );
        let uplink = loaded.robot.bus.uplink.expect("uplink parsed");
        assert_eq!(uplink.connect, "tls/cloud.phoxal.example:7447");
        assert_eq!(uplink.retry.initial_ms, 2000);
        assert_eq!(uplink.retry.max_ms, 10000);
        assert_eq!(
            uplink.auth.expect("auth").cert,
            PathBuf::from("identity/robot.pem")
        );
        Ok(())
    }

    #[test]
    fn load_robot_rejects_invalid_launch_ids_and_collisions() -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        let path = temp.path().join("robot.yaml");
        std::fs::write(
            &path,
            r#"schema: v0
robot:
  id: bot
  namespace: dev
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
            generation: "y2026_1".to_string(),
            version: "0.1.0".to_string(),
            artifact_ref: "service-asset:y2026_1-stable".to_string(),
            sha256: None,
            metadata: None,
            target_status: Some(ArtifactStatus::Pending),
            per_triple_status: BTreeMap::new(),
            changed_contracts: Vec::new(),
            contract_uses: Vec::new(),
            path_override: None,
        };

        assert_eq!(runtime.artifact_ref(), "service-asset:y2026_1-stable");
    }
}
