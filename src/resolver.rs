use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use phoxal::model::robot::{
    RobotV1 as Robot,
    v1::{Channel, ComponentSource, UserParticipant},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::catalog::{
    ArtifactKind, ArtifactStatus, CatalogEntry, CatalogRevision, Channel as CatalogChannel,
    ContractUse, DEFAULT_TOOL_VERSIONS, compare_generations, lookup_tool_version,
};
use crate::shell;
use crate::utils::{hash_tree, resolve_project_path};

const ROBOT_FILE: &str = "robot.yaml";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolveOptions {
    /// Resolve external artifact checksums from the network (tool asset sha256).
    /// Off for offline flows.
    pub resolve_external_artifacts: bool,
    /// Resolve git component `tag` → `commit`. A `tag` that is already a full
    /// commit SHA resolves with no network; a tag/branch ref is resolved live
    /// via `git ls-remote`. Flows that need to locate/stage component driver
    /// sources (`check`, `service run`, simulate, `deploy build`) set this;
    /// flows that never read component commits (`pull`, `outdated`) leave it
    /// off so they stay fully offline.
    pub resolve_source_commits: bool,
}

impl Default for ResolveOptions {
    fn default() -> Self {
        Self {
            resolve_external_artifacts: true,
            resolve_source_commits: true,
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
    pub user_runtimes: Vec<ResolvedUserRuntime>,
    pub components: Vec<ResolvedComponent>,
    pub tools: Vec<ResolvedTool>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct RobotManifestExtras {
    pub catalog_source: Option<PathBuf>,
    pub user_runtimes: BTreeMap<String, UserRuntimeManifestExtras>,
    pub bus: BusManifestExtras,
}

impl RobotManifestExtras {
    #[must_use]
    pub fn user_runtime_config(&self, runtime_name: &str) -> Option<&Value> {
        self.user_runtimes
            .get(runtime_name)
            .and_then(|runtime| runtime.config.as_ref())
    }

    #[must_use]
    pub fn materialized_bus_profile(&self, default_connect: &str) -> MaterializedBusProfile {
        self.bus.materialize(default_connect)
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct UserRuntimeManifestExtras {
    pub config: Option<Value>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct BusManifestExtras {
    pub selected_profile: Option<String>,
    pub profiles: BTreeMap<String, BusProfileConfig>,
}

impl BusManifestExtras {
    #[must_use]
    pub fn selected_profile_name(&self) -> &str {
        self.selected_profile
            .as_deref()
            .unwrap_or(DEFAULT_BUS_PROFILE_NAME)
    }

    #[must_use]
    pub fn materialize(&self, default_connect: &str) -> MaterializedBusProfile {
        let selected_profile = self.selected_profile_name().to_string();
        let configured = self.profiles.get(&selected_profile);
        MaterializedBusProfile {
            selected_profile,
            connect: configured
                .and_then(|profile| profile.connect.clone())
                .unwrap_or_else(|| default_connect.to_string()),
        }
    }
}

/// A manifest bus profile. Per the transport spec a profile is just
/// `{ connect: <endpoint> }`; the router-binding `listen` endpoint is deploy
/// infra (a CLI-side default), not a manifest profile field.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct BusProfileConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connect: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MaterializedBusProfile {
    pub selected_profile: String,
    pub connect: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LoadedRobot {
    pub robot: Robot,
    pub extras: RobotManifestExtras,
}

const DEFAULT_BUS_PROFILE_NAME: &str = "default";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedPlatformRuntime {
    pub name: String,
    pub artifact_id: String,
    pub kind: ArtifactKind,
    pub generation: String,
    pub version: String,
    pub artifact_ref: String,
    pub target_status: Option<ArtifactStatus>,
    pub per_triple_status: BTreeMap<String, ArtifactStatus>,
    pub changed_contracts: Vec<String>,
    pub contract_uses: Vec<ContractUse>,
}

impl ResolvedPlatformRuntime {
    /// The selected official service artifact identifier.
    #[must_use]
    pub fn artifact_ref(&self) -> &str {
        &self.artifact_ref
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

fn parse_robot_value_with_extras(yaml: &mut serde_yaml::Value, path: &Path) -> Result<LoadedRobot> {
    let (extras, _) = take_manifest_extras(yaml, path)?;
    let sanitized = serde_yaml::to_string(&yaml)
        .with_context(|| format!("failed to prepare {}", path.display()))?;
    let robot = Robot::read_from_string(&sanitized)?;

    Ok(LoadedRobot { robot, extras })
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

    if let Some(root) = yaml.as_mapping_mut()
        && let Some(bus) = root.remove("bus")
    {
        extras.bus = parse_bus_manifest_extras(&bus, robot_path)?;
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

fn parse_bus_manifest_extras(
    bus: &serde_yaml::Value,
    robot_path: &Path,
) -> Result<BusManifestExtras> {
    let bus = bus.as_mapping().ok_or_else(|| {
        anyhow!(
            "bus in {} must be a mapping with profiles",
            robot_path.display()
        )
    })?;
    for key in bus.keys().filter_map(serde_yaml::Value::as_str) {
        if !matches!(
            key,
            "profiles" | "selected" | "profile" | "default" | "default_profile"
        ) {
            bail!("bus.{key} in {} is not supported", robot_path.display());
        }
    }

    // `bus.profiles` is optional: the `default` profile (local Zenoh) is
    // implicit, so a manifest may declare only non-default profiles, or omit
    // the block entirely. Declare only what differs from the implicit default.
    let profiles = match bus.get("profiles") {
        Some(profiles_value) => parse_bus_profiles(profiles_value, robot_path)?,
        None => BTreeMap::new(),
    };
    let selected_profile = parse_selected_bus_profile(bus, robot_path)?;
    // Only an explicitly named, non-default selected profile must exist: the
    // implicit `default` never needs to be declared in bus.profiles.
    if let Some(selected_name) = selected_profile.as_deref()
        && selected_name != DEFAULT_BUS_PROFILE_NAME
        && !profiles.contains_key(selected_name)
    {
        let available = profiles.keys().cloned().collect::<Vec<_>>().join(", ");
        bail!(
            "bus selected profile '{selected_name}' in {} is not defined in bus.profiles; available: {available}",
            robot_path.display()
        );
    }

    Ok(BusManifestExtras {
        selected_profile,
        profiles,
    })
}

fn parse_bus_profiles(
    profiles: &serde_yaml::Value,
    robot_path: &Path,
) -> Result<BTreeMap<String, BusProfileConfig>> {
    let profiles = profiles.as_mapping().ok_or_else(|| {
        anyhow!(
            "bus.profiles in {} must be a mapping of profile names",
            robot_path.display()
        )
    })?;
    if profiles.is_empty() {
        bail!(
            "bus.profiles in {} must define at least one profile",
            robot_path.display()
        );
    }

    let mut parsed = BTreeMap::new();
    for (name, profile) in profiles {
        let Some(name) = name.as_str() else {
            continue;
        };
        validate_bus_profile_name(name)?;
        let profile = parse_bus_profile_config(name, profile, robot_path)?;
        parsed.insert(name.to_string(), profile);
    }
    Ok(parsed)
}

fn parse_bus_profile_config(
    name: &str,
    profile: &serde_yaml::Value,
    robot_path: &Path,
) -> Result<BusProfileConfig> {
    let profile = profile.as_mapping().ok_or_else(|| {
        anyhow!(
            "bus.profiles.{name} in {} must be a mapping",
            robot_path.display()
        )
    })?;
    for key in profile.keys().filter_map(serde_yaml::Value::as_str) {
        // A profile is just `{ connect }` per the transport spec; `listen` is
        // router-binding deploy infra, not a manifest profile field.
        if key != "connect" {
            bail!(
                "bus.profiles.{name}.{key} in {} is not supported",
                robot_path.display()
            );
        }
    }
    Ok(BusProfileConfig {
        connect: profile
            .get("connect")
            .map(|value| parse_bus_endpoint(name, "connect", value, robot_path))
            .transpose()?,
    })
}

fn parse_bus_endpoint(
    profile_name: &str,
    field: &str,
    value: &serde_yaml::Value,
    robot_path: &Path,
) -> Result<String> {
    let Some(endpoint) = value.as_str() else {
        bail!(
            "bus.profiles.{profile_name}.{field} in {} must be a string",
            robot_path.display()
        );
    };
    if endpoint.trim().is_empty() {
        bail!(
            "bus.profiles.{profile_name}.{field} in {} must not be empty",
            robot_path.display()
        );
    }
    Ok(endpoint.to_string())
}

fn parse_selected_bus_profile(
    bus: &serde_yaml::Mapping,
    robot_path: &Path,
) -> Result<Option<String>> {
    let mut selected = None;
    for key in ["selected", "profile", "default", "default_profile"] {
        let Some(value) = bus.get(key) else {
            continue;
        };
        let Some(name) = value.as_str() else {
            bail!("bus.{key} in {} must be a string", robot_path.display());
        };
        validate_bus_profile_name(name)?;
        if let Some(existing) = &selected
            && existing != name
        {
            bail!(
                "bus profile selectors in {} disagree: '{existing}' and '{name}'",
                robot_path.display()
            );
        }
        selected = Some(name.to_string());
    }
    Ok(selected)
}

fn validate_bus_profile_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.contains('/')
        || name.contains('\\')
        || name == "."
        || name == ".."
        || name.chars().any(char::is_whitespace)
    {
        bail!("bus profile name '{name}' is invalid; use a simple name such as `default` or `dev`");
    }
    Ok(())
}

pub fn resolve(
    robot: &Robot,
    project_root: &Path,
    catalog: Option<&CatalogRevision>,
    options: ResolveOptions,
) -> Result<ResolvedRobot> {
    let channel = robot.phoxal_artifacts.channel;
    let catalog_channel = CatalogChannel::from(channel);
    let target = robot
        .phoxal_artifacts
        .target
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
    if !robot.phoxal_participants.images.is_empty() {
        bail!(
            "phoxal_participants.images is no longer supported: official artifacts now ship as native release assets"
        );
    }

    let platform_runtimes = catalog
        .map(|catalog| {
            resolve_catalog_entries(catalog, catalog_channel, &target, &target_generation)
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
    let components = resolve_components(robot, options.resolve_source_commits)?;
    let tools = resolve_tools(robot, options.resolve_external_artifacts)?;

    Ok(ResolvedRobot {
        robot: robot.clone(),
        target_generation,
        channel,
        target,
        catalog_revision: catalog.map(|catalog| catalog.revision.clone()),
        platform_runtimes,
        user_runtimes,
        components,
        tools,
    })
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
    if let Some(catalog) = catalog
        && let Some(generation) = catalog.newest_generation_on_channel(channel, target)
    {
        return Ok(generation);
    }
    if robot.user_participants.is_empty()
        && robot
            .components
            .instances
            .values()
            .all(|component| component.driver.is_none())
    {
        bail!("{}", crate::catalog::unavailable_catalog_error());
    }
    Ok("source".to_string())
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
            let artifact_ref = entry
                .release_assets
                .get(target)
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
                target_status: entry.status_for(target),
                per_triple_status: entry.status.clone(),
                changed_contracts: entry.changed_contracts.clone(),
                contract_uses: entry.contract_uses.clone(),
            })
        })
        .collect()
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
        });
    }
    Ok(components)
}

fn resolve_tools(robot: &Robot, resolve_external_artifacts: bool) -> Result<Vec<ResolvedTool>> {
    // Reject robot.yaml tool selectors for unknown tools up front (previously a
    // robot.tools entry seeded the request map; now the catalog is the source of
    // truth and robot.yaml may only pin known tools to explicit versions).
    for name in robot.tools.keys() {
        if lookup_tool_version(name).is_none() {
            bail!("unknown native tool '{name}'");
        }
    }
    let target = host_target_triple();
    DEFAULT_TOOL_VERSIONS
        .iter()
        .map(|catalog_tool| {
            let name = catalog_tool.name;
            let override_version = robot.tools.get(name).map(|tool| tool.version.as_str());
            let version = override_version
                .unwrap_or(catalog_tool.default_version)
                .to_string();
            let asset = render_tool_template(catalog_tool.artifact_template, &version, &target);
            let binary_name = render_tool_template(catalog_tool.binary_template, &version, &target);
            let sha256 = if resolve_external_artifacts {
                resolve_release_asset_sha256(catalog_tool.repo, &version, &asset)?
                    .unwrap_or_default()
            } else {
                fake_sha(&format!("{name}:{version}:{asset}"))
            };
            Ok(ResolvedTool {
                name: name.to_string(),
                requested: version.clone(),
                resolved: version,
                repo: catalog_tool.repo.to_string(),
                asset,
                binary_name,
                sha256,
            })
        })
        .collect()
}

fn render_tool_template(template: &str, version: &str, target: &str) -> String {
    template
        .replace("{version}", version)
        .replace("{target}", target)
}

pub(crate) fn resolve_release_asset_sha256(
    repo: &str,
    version: &str,
    asset: &str,
) -> Result<Option<String>> {
    let url = format!("https://api.github.com/repos/{repo}/releases/tags/v{version}");
    let client = reqwest::blocking::Client::builder()
        .user_agent("phoxal-cli")
        .timeout(Duration::from_secs(15))
        .build()?;
    let mut req = client.get(&url);
    if let Ok(token) = std::env::var("GITHUB_TOKEN") {
        req = req.bearer_auth(token);
    }
    let response = req
        .send()
        .with_context(|| format!("failed to fetch GitHub release {repo} v{version}"))?;
    if !response.status().is_success() {
        let status = response.status();
        if is_unpublished_placeholder(version) {
            return Ok(None);
        }
        if status.as_u16() == 403 {
            bail!(
                "GitHub releases API returned 403 while resolving {repo} v{version} \
                 (likely anonymous rate limit of 60/hour from this network - wait an hour, \
                 or set GITHUB_TOKEN env var for 5000/hour)"
            );
        }
        bail!("GitHub release lookup for {repo} v{version} returned {status}");
    }

    #[derive(Deserialize)]
    struct GhAsset {
        name: String,
        digest: Option<String>,
    }

    #[derive(Deserialize)]
    struct GhRelease {
        assets: Vec<GhAsset>,
    }

    let release: GhRelease = response
        .json()
        .with_context(|| format!("failed to parse GitHub release {repo} v{version}"))?;
    let Some(release_asset) = release
        .assets
        .iter()
        .find(|candidate| candidate.name == asset)
    else {
        if is_unpublished_placeholder(version) {
            return Ok(None);
        }
        bail!("GitHub release {repo} v{version} does not contain asset {asset}");
    };
    let Some(digest) = release_asset.digest.as_deref() else {
        if is_unpublished_placeholder(version) {
            return Ok(None);
        }
        bail!("GitHub release asset {repo} v{version} {asset} does not expose a digest");
    };
    let sha256 = digest.strip_prefix("sha256:").unwrap_or(digest);
    if sha256.len() == 64 && sha256.chars().all(|value| value.is_ascii_hexdigit()) {
        Ok(Some(sha256.to_ascii_lowercase()))
    } else if is_unpublished_placeholder(version) {
        Ok(None)
    } else {
        bail!("GitHub release asset {repo} v{version} {asset} has invalid sha256 digest {digest}")
    }
}

fn is_unpublished_placeholder(version: &str) -> bool {
    version == "0.0.0-dev"
}

/// Deterministic non-cryptographic stand-in hash for **tool asset** entries
/// during offline resolution.
fn fake_sha(seed: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(seed.as_bytes());
    hex::encode(hasher.finalize())
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
                resolve_external_artifacts: false,
                resolve_source_commits: false,
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
                resolve_external_artifacts: false,
                resolve_source_commits: true,
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
    fn load_robot_extracts_bus_profiles() -> anyhow::Result<()> {
        // Per the transport spec the `default` profile is implicit and a
        // profile is just `{ connect }`; declare only the non-default profile.
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
  selected: lab
  profiles:
    lab:
      connect: tcp/lab-router:7447
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
        let materialized = loaded.extras.materialized_bus_profile("tcp/router:7447");

        assert_eq!(materialized.selected_profile, "lab");
        assert_eq!(materialized.connect, "tcp/lab-router:7447");
        Ok(())
    }

    #[test]
    fn load_robot_implicit_default_profile_needs_no_declaration() -> anyhow::Result<()> {
        // No `bus` block at all: the implicit `default` profile materializes to
        // the CLI-provided default connect endpoint.
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
        let materialized = loaded.extras.materialized_bus_profile("tcp/router:7447");

        assert_eq!(materialized.selected_profile, "default");
        assert_eq!(materialized.connect, "tcp/router:7447");
        Ok(())
    }

    #[test]
    fn load_robot_rejects_listen_as_profile_field() -> anyhow::Result<()> {
        // `listen` is router-binding deploy infra, not a manifest profile field.
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
  profiles:
    lab:
      connect: tcp/lab-router:7447
      listen: tcp/0.0.0.0:7448
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

        let error =
            load_robot_with_extras(&path).expect_err("listen is not a manifest profile field");
        assert!(
            error.to_string().contains("bus.profiles.lab.listen"),
            "{error:#}"
        );
        Ok(())
    }

    #[test]
    fn load_robot_rejects_missing_selected_bus_profile() -> anyhow::Result<()> {
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
  selected: missing
  profiles:
    lab:
      connect: tcp/lab-router:7447
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

        let error =
            load_robot_with_extras(&path).expect_err("selected profile must exist in bus.profiles");
        assert!(
            error.to_string().contains("bus selected profile 'missing'"),
            "{error:#}"
        );
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
            target_status: Some(ArtifactStatus::Pending),
            per_triple_status: BTreeMap::new(),
            changed_contracts: Vec::new(),
            contract_uses: Vec::new(),
        };

        assert_eq!(runtime.artifact_ref(), "service-asset:y2026_1-stable");
    }
}
