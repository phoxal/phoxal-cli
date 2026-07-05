use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use phoxal::model::robot::{
    RobotV1 as Robot,
    v1::{ArtifactPin, Channel, ComponentSource, UserParticipant},
};
use serde_json::Value;

use crate::catalog::{
    ArtifactKind, ArtifactStatus, CatalogEntry, CatalogRevision, Channel as CatalogChannel,
    ContractUse, ReleaseAssetMetadata, compare_generations,
};
use crate::shell;
use crate::utils::{hash_tree, resolve_project_path};

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedPlatformRuntime {
    pub name: String,
    pub artifact_id: String,
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
    pub framework: String,
    pub source_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedComponent {
    pub instance: String,
    pub source_name: String,
    pub source: ResolvedComponentSource,
    pub has_driver: bool,
    pub driver_path_override: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolvedComponentSource {
    Git {
        git: String,
        tag: String,
        commit: String,
        /// Subdirectory within the repository holding the component
        /// definition. `None` means the repository root.
        directory: Option<PathBuf>,
    },
    Path {
        path: PathBuf,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedTool {
    pub name: String,
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
    Driver,
    Tool,
    Simulator,
}

impl ResolvedPathOverrideKind {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Service => "service",
            Self::Driver => "driver",
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
    // `user_participants.<name>.config` keys as a CLI-side side channel: they
    // are stripped before the typed parse and threaded through
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
        .and_then(|root| root.get("phoxal_artifacts"))
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
        "{path}: phoxal_artifacts.pins path overrides are dev-overlay only; move {} to robot.<env>.yaml and load it with --env <env>",
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
    for name in robot.user_participants.keys() {
        if !is_launch_id(name) {
            errors.push(format!(
                "user_participants.{name} in {} must use only [a-z0-9_]; '-' is reserved as a launch separator",
                path.display()
            ));
        }
    }
    for instance in robot.components.instances.keys() {
        if !is_launch_id(instance) {
            errors.push(format!(
                "components.instances.{instance} in {} must use only [a-z0-9_]; '-' is reserved as a launch separator",
                path.display()
            ));
        }
        if robot.user_participants.contains_key(instance) {
            errors.push(format!(
                "user_participants.{instance} collides with components.instances.{instance}; participant ids must be unique",
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
    if is_replace_whole_user_runtime_config(path) {
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

fn is_replace_whole_user_runtime_config(path: &[String]) -> bool {
    path.len() == 3 && path[0] == "user_participants" && path[2] == "config"
}

fn take_manifest_extras(
    yaml: &mut serde_yaml::Value,
    robot_path: &Path,
) -> Result<(RobotManifestExtras, bool)> {
    let mut extras = RobotManifestExtras::default();
    let mut stripped_extras = false;

    if let Some(root) = yaml.as_mapping_mut()
        && let Some(artifacts) = root.get_mut("phoxal_artifacts")
        && let Some(artifacts) = artifacts.as_mapping_mut()
        && let Some(catalog) = artifacts.remove("catalog")
    {
        extras.catalog_source = Some(parse_catalog_source_extra(&catalog, robot_path)?);
        stripped_extras = true;
    }

    let Some(user_runtimes) = yaml
        .as_mapping_mut()
        .and_then(|mapping| mapping.get_mut("user_participants"))
        .and_then(serde_yaml::Value::as_mapping_mut)
    else {
        return Ok((extras, stripped_extras));
    };

    for (name, runtime) in user_runtimes {
        let Some(name) = name.as_str() else {
            continue;
        };
        let Some(runtime) = runtime.as_mapping_mut() else {
            continue;
        };
        let config = runtime.remove("config");
        stripped_extras |= config.is_some();
        let config = config
            .map(|config| {
                serde_json::to_value(config).with_context(|| {
                    format!("user_participants.{name}.config must be representable as JSON")
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
            "phoxal_artifacts.catalog in {} must be a local path string",
            robot_path.display()
        );
    };
    if source.trim().is_empty() {
        bail!(
            "phoxal_artifacts.catalog in {} must not be empty",
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
    let channel = robot.phoxal_artifacts.channel;
    let catalog_channel = CatalogChannel::from(channel);
    let target = options
        .official_target_triple
        .clone()
        .or_else(|| robot.phoxal_artifacts.target.clone())
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
    if !robot.phoxal_participants.images.is_empty() {
        bail!(
            "phoxal_participants.images is no longer supported: official artifacts now ship as native release assets"
        );
    }
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
        .user_participants
        .iter()
        .map(|(name, runtime)| {
            resolve_user_runtime(project_root, &target_generation, name, runtime)
        })
        .collect::<Result<Vec<_>>>()?;

    // Git ref → commit SHA resolution: when `resolve_source_commits` is set, a
    // `tag` that is already a full commit SHA resolves with no network, while a
    // tag/branch ref is resolved live via `git ls-remote`. Flows that never read
    // component commits (`pull`, `outdated`) leave it off so they stay offline.
    let mut components = resolve_components(robot, options.resolve_source_commits)?;
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
    for (key, pin) in &robot.phoxal_artifacts.pins {
        let ArtifactPin::Path(pin) = pin;
        let path = resolve_project_path(project_root, &pin.path);
        if apply_service_path_pin(key, &path, platform_runtimes, &mut overrides) {
            continue;
        }
        if apply_driver_path_pin(key, &path, components, &mut overrides) {
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
        .find(|runtime| runtime.kind == ArtifactKind::Service && runtime.artifact_id == key)
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

fn apply_driver_path_pin(
    key: &str,
    path: &Path,
    components: &mut [ResolvedComponent],
    overrides: &mut Vec<ResolvedPathOverride>,
) -> bool {
    let Some(driver_name) = key.strip_prefix("driver-") else {
        return false;
    };
    let mut used = false;
    for component in components
        .iter_mut()
        .filter(|component| component.has_driver && component.source_name == driver_name)
    {
        component.driver_path_override = Some(path.to_path_buf());
        used = true;
    }
    if used {
        overrides.push(ResolvedPathOverride {
            key: key.to_string(),
            kind: ResolvedPathOverrideKind::Driver,
            artifact_name: driver_name.to_string(),
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
    let Some(tool) = tools.iter_mut().find(|tool| tool.name == key) else {
        return false;
    };
    tool.path_override = Some(path.to_path_buf());
    tool.asset = format!("path:{}", path.display());
    tool.sha256 = crate::utils::hash_tree(path).unwrap_or_default();
    tool.metadata = None;
    overrides.push(ResolvedPathOverride {
        key: key.to_string(),
        kind: ResolvedPathOverrideKind::Tool,
        artifact_name: tool_emit_apis_id(key).to_string(),
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
        .find(|runtime| runtime.kind == ArtifactKind::Simulator && runtime.artifact_id == key)
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
    if matches!(
        key.split_once('-').map(|(prefix, _)| prefix),
        Some("service" | "driver" | "tool" | "simulator")
    ) {
        format!(
            "unused artifact path pin '{key}': no artifact with that kind-qualified id is used by the resolved graph; available path-pin ids: {available}"
        )
    } else {
        format!(
            "unknown artifact path pin '{key}': pins must be kind-qualified ids (service-*, driver-*, tool-*, simulator-*); available path-pin ids: {available}"
        )
    }
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
            .map(|runtime| runtime.artifact_id.clone()),
    );
    keys.extend(
        components
            .iter()
            .filter(|component| component.has_driver)
            .map(|component| format!("driver-{}", component.source_name)),
    );
    keys.extend(tools.iter().map(|tool| tool.name.clone()));
    keys.extend(
        simulators
            .iter()
            .map(|simulator| simulator.artifact_id.clone()),
    );
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
    if let Some(generation) = robot.phoxal_artifacts.generation.as_deref() {
        validate_generation_pin(robot.api_version.as_deref(), generation)?;
        return Ok(generation.to_string());
    }
    if let Some(generation) = robot.api_version.as_deref() {
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
    robot.user_participants.is_empty()
        && robot
            .components
            .instances
            .values()
            .all(|component| component.driver.is_none())
}

fn catalog_target_generation_not_yet_available(
    catalog: &CatalogRevision,
    channel: CatalogChannel,
    target: &str,
) -> anyhow::Error {
    anyhow!(
        "NotYetAvailable: loaded artifact catalog revision {} has no released generation assets for target {target} on channel {channel}. Pass a catalog that includes released assets for {target}/{channel}, pin phoxal_artifacts.generation/api_version for source-only development, or deploy a target covered by this catalog.",
        catalog.revision
    )
}

pub(crate) fn target_generation_for_robot(
    robot: &Robot,
    catalog: Option<&CatalogRevision>,
) -> Result<String> {
    let target = robot
        .phoxal_artifacts
        .target
        .clone()
        .unwrap_or_else(host_target_triple);
    target_generation(
        robot,
        catalog,
        CatalogChannel::from(robot.phoxal_artifacts.channel),
        &target,
    )
}

fn validate_generation_pin(root_api_version: Option<&str>, generation: &str) -> Result<()> {
    if let Some(root_api_version) = root_api_version
        && root_api_version != generation
    {
        bail!(
            "robot.yaml declares api_version {root_api_version} but phoxal_artifacts.generation {generation}; remove the root api_version or make both generations match"
        );
    }
    Ok(())
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
            .entry(entry.artifact_id.clone())
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
                .ok_or_else(|| anyhow!("{} does not match kind {}", entry.artifact_id, entry.kind))?
                .to_string();
            let release_asset = entry.release_assets.get(target);
            let artifact_ref = release_asset
                .map(|asset| asset.asset.clone())
                .unwrap_or_else(|| {
                    format!(
                        "{}:{}-{}-{}-{}",
                        entry.artifact_id, entry.version, entry.api_generation, channel, target
                    )
                });
            Ok(ResolvedPlatformRuntime {
                name,
                artifact_id: entry.artifact_id.clone(),
                kind: entry.kind,
                generation: entry.api_generation.clone(),
                version: entry.version.clone(),
                artifact_ref,
                sha256: release_asset.map(|asset| asset.sha256.clone()),
                metadata: release_asset.map(|asset| resolved_metadata(&asset.metadata)),
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
            .entry(entry.artifact_id.clone())
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
        .ok_or_else(|| anyhow!("{} does not match kind {}", entry.artifact_id, entry.kind))?
        .to_string();
    let release_asset = entry.release_assets.get(target);
    let artifact_ref = release_asset
        .map(|asset| asset.asset.clone())
        .unwrap_or_else(|| {
            format!(
                "{}:{}-{}-{}",
                entry.artifact_id, entry.version, entry.api_generation, target
            )
        });
    Ok(ResolvedPlatformRuntime {
        name,
        artifact_id: entry.artifact_id.clone(),
        kind: entry.kind,
        generation: entry.api_generation.clone(),
        version: entry.version.clone(),
        artifact_ref,
        sha256: release_asset.map(|asset| asset.sha256.clone()),
        metadata: release_asset.map(|asset| resolved_metadata(&asset.metadata)),
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
                .push(&entry.artifact_id);
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

/// Resolve a git component `tag` to a concrete commit SHA.
///
/// A `tag` that is already a full 40-character commit SHA is an explicit pin and
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
             Pin the component to an explicit commit SHA in robot.yaml (components.sources.<name>.tag: <40-char sha>), \
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
    api_version: &str,
    name: &str,
    runtime: &UserParticipant,
) -> Result<ResolvedUserRuntime> {
    let runtime_dir = resolve_project_path(project_root, &runtime.path);
    if !runtime_dir.is_dir() {
        bail!(
            "user service '{name}' source dir {} does not exist; user services must have an on-disk source directory to hash/build",
            runtime_dir.display()
        );
    }
    let framework = resolve_user_runtime_framework(name, &runtime.framework, api_version)?;
    let source_hash = hash_tree(&runtime_dir).with_context(|| {
        format!(
            "failed to hash user service '{name}' source tree at {}",
            runtime_dir.display()
        )
    })?;
    Ok(ResolvedUserRuntime {
        name: name.to_string(),
        path: runtime.path.clone(),
        framework,
        source_hash,
    })
}

pub(crate) fn resolve_user_runtime_framework(
    runtime_name: &str,
    selector: &str,
    api_version: &str,
) -> Result<String> {
    validate_user_runtime_framework_selector(runtime_name, selector, api_version)?;
    if selector == "match-platform" {
        return Ok(api_version.to_string());
    }
    Ok(selector.to_string())
}

pub(crate) fn validate_user_runtime_framework_selector(
    runtime_name: &str,
    selector: &str,
    target_generation: &str,
) -> Result<()> {
    if selector == "match-platform" || selector == target_generation {
        return Ok(());
    }
    bail!(
        "user service '{runtime_name}': framework '{selector}' must be \"match-platform\" or the target generation '{target_generation}'"
    )
}

fn resolve_components(
    robot: &Robot,
    resolve_source_commits: bool,
) -> Result<Vec<ResolvedComponent>> {
    let mut components = Vec::new();
    for (instance_name, instance) in &robot.components.instances {
        let source = robot
            .components
            .sources
            .get(&instance.component)
            .ok_or_else(|| {
                anyhow!(
                    "components.instances.{}.component references missing source '{}'",
                    instance_name,
                    instance.component
                )
            })?;
        let source = match source {
            ComponentSource::Git(source) => {
                // Resolve the commit live. A `tag` that is already a full commit
                // SHA needs no network; a tag/branch ref is resolved via
                // `git ls-remote`. Flows that never read commits leave
                // `resolve_source_commits` off and skip this entirely.
                let commit = if resolve_source_commits {
                    resolve_component_commit(&source.git, &source.tag)?
                } else {
                    String::new()
                };
                ResolvedComponentSource::Git {
                    git: source.git.clone(),
                    tag: source.tag.clone(),
                    commit,
                    directory: source.directory.clone(),
                }
            }
            ComponentSource::Path(source) => ResolvedComponentSource::Path {
                path: source.path.clone(),
            },
        };
        components.push(ResolvedComponent {
            instance: instance_name.clone(),
            source_name: instance.component.clone(),
            source,
            has_driver: instance.driver.is_some(),
            driver_path_override: None,
        });
    }
    Ok(components)
}

fn resolve_tools(
    robot: &Robot,
    catalog: Option<&CatalogRevision>,
    channel: CatalogChannel,
    target: &str,
    target_generation: &str,
) -> Result<Vec<ResolvedTool>> {
    let Some(catalog) = catalog else {
        if let Some(name) = robot.tools.keys().next() {
            bail!("unknown native tool '{name}'");
        }
        return Ok(Vec::new());
    };

    let tool_entries = catalog
        .entries
        .iter()
        .filter(|entry| entry.kind == ArtifactKind::Tool)
        .collect::<Vec<_>>();
    for name in robot.tools.keys() {
        if !tool_entries.iter().any(|entry| entry.artifact_id == *name) {
            bail!("unknown native tool '{name}'");
        }
    }

    let mut selected = BTreeMap::<String, &CatalogEntry>::new();
    for entry in tool_entries {
        if !entry.target_triples.iter().any(|triple| triple == target) {
            continue;
        }
        if compare_generations(&entry.api_generation, target_generation).is_gt() {
            continue;
        }
        if let Some(pin) = robot.tools.get(&entry.artifact_id) {
            if entry.version != pin.version {
                continue;
            }
        } else if !entry.channels.contains_key(&channel) {
            continue;
        }
        selected
            .entry(entry.artifact_id.clone())
            .and_modify(|existing| {
                if compare_catalog_entries(entry, existing).is_gt() {
                    *existing = entry;
                }
            })
            .or_insert(entry);
    }

    for (name, pin) in &robot.tools {
        if !selected.contains_key(name) {
            bail!(
                "native tool '{name}' version {} is not available in the artifact catalog for target {target}",
                pin.version
            );
        }
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
                        entry.artifact_id, entry.version, entry.api_generation, target
                    )
                });
            Ok(ResolvedTool {
                name: entry.artifact_id.clone(),
                requested: robot
                    .tools
                    .get(&entry.artifact_id)
                    .map(|tool| tool.version.clone())
                    .unwrap_or_else(|| entry.version.clone()),
                resolved: entry.version.clone(),
                repo: "phoxal/framework".to_string(),
                asset,
                binary_name: official_binary_name(entry.kind, entry.artifact_name().unwrap_or("")),
                sha256: release_asset
                    .map(|asset| asset.sha256.clone())
                    .unwrap_or_else(|| "0".repeat(64)),
                metadata: release_asset.map(|asset| resolved_metadata(&asset.metadata)),
                path_override: None,
            })
        })
        .collect()
}

pub(crate) fn tool_emit_apis_id(tool_name: &str) -> &str {
    tool_name.strip_prefix("tool-").unwrap_or(tool_name)
}

pub(crate) fn official_binary_name(kind: ArtifactKind, name: &str) -> String {
    format!("phoxal-{kind}-{name}")
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
        fixture_contract_for_tests, fixture_service_entry_for_tests,
    };

    fn test_catalog() -> CatalogRevision {
        fixture_catalog_for_tests(vec![fixture_service_entry_for_tests(
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
        )])
    }

    #[test]
    fn resolve_without_source_commits_leaves_git_component_commits_empty() -> anyhow::Result<()> {
        // Flows that never read component commits (`pull`, `outdated`) resolve
        // with `resolve_source_commits: false` and must NOT run `git ls-remote`.
        // A git component is resolved with an empty commit; if resolution tried
        // to reach the network it would either hang or fail, so an empty commit
        // proves no ls-remote was attempted.
        let robot = Robot::parse_from_string(GIT_COMPONENT_ROBOT)?;
        let catalog = test_catalog();
        let resolved = resolve(
            &robot,
            std::path::Path::new("."),
            Some(&catalog),
            ResolveOptions {
                resolve_source_commits: false,
                ..ResolveOptions::default()
            },
        )?;

        let git_component = resolved
            .components
            .iter()
            .find(|component| component.source_name == "ddsm115")
            .expect("ddsm115 component resolved");
        match &git_component.source {
            ResolvedComponentSource::Git { commit, tag, .. } => {
                assert_eq!(tag, "main");
                assert!(
                    commit.is_empty(),
                    "offline resolve must leave the git commit empty (no ls-remote), got {commit:?}"
                );
            }
            other => panic!("expected a git component source, got {other:?}"),
        }
        Ok(())
    }

    const GIT_COMPONENT_ROBOT: &str = r#"schema: v0
api_version: y2026_1
identity:
  id: testbot
  namespace: test
structure: structure.urdf
phoxal_artifacts:
  channel: stable
phoxal_participants: {}
motion:
  kinematic:
    kind: differential
    left_actuators: [left_drive.motor]
    right_actuators: [right_drive.motor]
    left_encoders: [left_drive.encoder]
    right_encoders: [right_drive.encoder]
    wheel_radius_m: 0.1
    wheel_base_m: 0.5
components:
  sources:
    ddsm115:
      git: https://github.com/phoxal/framework
      tag: main
      directory: component/ddsm115
  instances:
    left_drive:
      component: ddsm115
      mount_link: left_wheel_mount
    right_drive:
      component: ddsm115
      mount_link: right_wheel_mount
"#;

    #[test]
    fn explicit_commit_sha_tag_resolves_without_network() -> anyhow::Result<()> {
        // A `tag` that is already a full commit SHA is an explicit pin: it must
        // resolve with no network (no `git ls-remote`), so a live-resolution
        // flow works offline when components are pinned to a SHA.
        let sha = "0123456789abcdef0123456789abcdef01234567";
        let robot = Robot::parse_from_string(
            &GIT_COMPONENT_ROBOT.replace("tag: main", &format!("tag: {sha}")),
        )?;
        let catalog = test_catalog();
        let resolved = resolve(
            &robot,
            std::path::Path::new("."),
            Some(&catalog),
            ResolveOptions {
                resolve_source_commits: true,
                ..ResolveOptions::default()
            },
        )?;

        let git_component = resolved
            .components
            .iter()
            .find(|component| component.source_name == "ddsm115")
            .expect("ddsm115 component resolved");
        match &git_component.source {
            ResolvedComponentSource::Git { commit, tag, .. } => {
                assert_eq!(tag, sha);
                assert_eq!(commit, sha);
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
    fn load_robot_tolerates_user_runtime_config() -> anyhow::Result<()> {
        // The CLI threads `user_participants.<name>.config` through
        // `RobotManifestExtras` as a side channel; `load_robot` must strip it
        // so every command accepts a manifest that declares typed config.
        let temp = tempfile::tempdir()?;
        let path = temp.path().join("robot.yaml");
        std::fs::write(
            &path,
            r#"schema: v0
api_version: y2026_1
identity:
  id: bot
  namespace: dev
structure: structure.urdf
phoxal_artifacts:
  channel: stable
phoxal_participants: {}
user_participants:
  brain:
    path: runtimes/brain
    config:
      gain: 0.5
motion:
  kinematic:
    kind: differential
    left_actuators: [l.motor]
    right_actuators: [r.motor]
    left_encoders: [l.encoder]
    right_encoders: [r.encoder]
    wheel_radius_m: 0.1
    wheel_base_m: 0.5
components:
  sources: {}
  instances: {}
"#,
        )?;

        // Plain load_robot must parse it despite the config key the typed model
        // does not know about.
        let robot = load_robot(&path)?;
        assert!(robot.user_participants.contains_key("brain"));

        let loaded = load_robot_with_extras(&path)?;
        assert!(loaded.robot.user_participants.contains_key("brain"));
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
api_version: y2026_1
identity:
  id: bot
  namespace: dev
structure: structure.urdf
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
phoxal_artifacts:
  channel: stable
phoxal_participants: {}
motion:
  kinematic:
    kind: differential
    left_actuators: [l.motor]
    right_actuators: [r.motor]
    left_encoders: [l.encoder]
    right_encoders: [r.encoder]
    wheel_radius_m: 0.1
    wheel_base_m: 0.5
components:
  sources: {}
  instances: {}
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
api_version: y2026_1
identity:
  id: bot
  namespace: dev
structure: structure.urdf
phoxal_artifacts:
  channel: stable
phoxal_participants: {}
user_participants:
  mission-service:
    path: runtimes/mission
  left_drive:
    path: runtimes/left_drive
motion:
  kinematic:
    kind: differential
    left_actuators: [left_drive.motor]
    right_actuators: [right_drive.encoder]
    left_encoders: [left_drive.encoder]
    right_encoders: [right_drive.encoder]
    wheel_radius_m: 0.1
    wheel_base_m: 0.5
components:
  sources:
    ddsm115:
      path: components/ddsm115
  instances:
    left_drive:
      component: ddsm115
      mount_link: left
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
            artifact_id: "service-asset".to_string(),
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
