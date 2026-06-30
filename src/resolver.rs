use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use phoxal::model::robot::{
    RobotV1 as Robot,
    v1::{Channel, ComponentSource, UserRuntime},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::catalog::{DEFAULT_TOOL_VERSIONS, PlatformRuntimeCatalog, lookup_tool_version};
use crate::shell;
use crate::utils::{hash_tree, resolve_project_path};

const ROBOT_FILE: &str = "robot.yaml";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolveOptions {
    /// Resolve external artifact digests/checksums from the network
    /// (registry image digests, tool asset sha256). Off for offline flows.
    pub resolve_external_artifacts: bool,
    /// Resolve git component `tag` → `commit`. A `tag` that is already a full
    /// commit SHA resolves with no network; a tag/branch ref is resolved live
    /// via `git ls-remote`. Flows that need to locate/stage component driver
    /// sources (`check`, `runtime run`, simulate, `deploy build`) set this;
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
    pub api_version: String,
    pub channel: Channel,
    pub platform_runtimes: Vec<ResolvedPlatformRuntime>,
    pub user_runtimes: Vec<ResolvedUserRuntime>,
    pub components: Vec<ResolvedComponent>,
    pub tools: Vec<ResolvedTool>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct RobotManifestExtras {
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
    pub fn user_runtime_image(&self, runtime_name: &str) -> Option<&str> {
        self.user_runtimes
            .get(runtime_name)
            .and_then(|runtime| runtime.image.as_deref())
    }

    #[must_use]
    pub fn materialized_bus_profile(&self, default_connect: &str) -> MaterializedBusProfile {
        self.bus.materialize(default_connect)
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct UserRuntimeManifestExtras {
    pub image: Option<String>,
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

/// How a platform runtime image is referenced for deployment.
///
/// A [`ImagePin::Digest`] is a reproducible OCI content pin obtained from the
/// registry (via `docker buildx imagetools inspect`). [`ImagePin::Unpinned`]
/// means no digest was resolved — the image is deployed by its selected tag
/// tag. We never fabricate a digest: an unpinned image is deployed by tag,
/// which is the honest pre-publish-recovery behavior and is still a real,
/// pullable reference (it just isn't content-addressed).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImagePin {
    /// A real registry-reported content digest, e.g. `sha256:…`.
    Digest(String),
    /// No digest resolved; deploy by `repo:version` tag.
    Unpinned,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedPlatformRuntime {
    pub name: String,
    pub image_ref: String,
    pub pin: ImagePin,
}

impl ResolvedPlatformRuntime {
    /// The selected image reference, e.g. `ghcr.io/phoxal/runtime-drive:y2026_1-stable`.
    #[must_use]
    pub fn tag_ref(&self) -> String {
        self.image_ref.clone()
    }

    /// The reference Docker should pull and compose should embed: a real
    /// digest pin when one was resolved, otherwise the selected image ref.
    /// Never a fabricated digest — so live `simulate` can never attempt to
    /// pull a fake `ref@sha256:…`.
    #[must_use]
    pub fn deploy_ref(&self) -> String {
        if image_ref_digest(&self.image_ref).is_some() {
            return self.image_ref.clone();
        }
        match &self.pin {
            ImagePin::Digest(digest) => format!("{}@{}", self.image_ref, digest),
            ImagePin::Unpinned => self.image_ref.clone(),
        }
    }

    /// The real content digest, if one was resolved. `None` during
    /// pre-publish recovery (deploy-by-tag).
    #[must_use]
    pub fn digest_pin(&self) -> Option<&str> {
        match &self.pin {
            ImagePin::Digest(digest) => Some(digest.as_str()),
            ImagePin::Unpinned => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedUserRuntime {
    pub name: String,
    pub path: PathBuf,
    pub framework: String,
    pub build: Option<ResolvedUserRuntimeBuild>,
    pub source_hash: String,
    pub image: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedUserRuntimeBuild {
    pub context: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dockerfile: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
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
    // `user_runtimes.<name>.image`/`config` keys: the typed `phoxal` model uses
    // `deny_unknown_fields` and does not carry those fields, so they must be
    // stripped before the typed parse. Commands that don't need the extras
    // (check/validate/update/runtime add) just discard them.
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
    path.len() == 3 && path[0] == "user_runtimes" && path[2] == "config"
}

fn take_manifest_extras(
    yaml: &mut serde_yaml::Value,
    robot_path: &Path,
) -> Result<(RobotManifestExtras, bool)> {
    let mut extras = RobotManifestExtras::default();
    let mut stripped_extras = false;

    if let Some(root) = yaml.as_mapping_mut()
        && let Some(bus) = root.remove("bus")
    {
        extras.bus = parse_bus_manifest_extras(&bus, robot_path)?;
        stripped_extras = true;
    }

    let Some(user_runtimes) = yaml
        .as_mapping_mut()
        .and_then(|mapping| mapping.get_mut("user_runtimes"))
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
        let image = runtime.remove("image");
        let config = runtime.remove("config");
        stripped_extras |= image.is_some() || config.is_some();

        let image = image
            .map(|image| {
                image.as_str().map(ToString::to_string).ok_or_else(|| {
                    anyhow!(
                        "user_runtimes.{name}.image in {} must be a string",
                        robot_path.display()
                    )
                })
            })
            .transpose()?;
        let config = config
            .map(|config| {
                serde_json::to_value(config).with_context(|| {
                    format!("user_runtimes.{name}.config must be representable as JSON")
                })
            })
            .transpose()?;

        if image.is_some() || config.is_some() {
            extras.user_runtimes.insert(
                name.to_string(),
                UserRuntimeManifestExtras { image, config },
            );
        }
    }

    Ok((extras, stripped_extras))
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
    catalog: &PlatformRuntimeCatalog,
    options: ResolveOptions,
) -> Result<ResolvedRobot> {
    let api_version = robot.api_version.clone();
    let channel = robot.phoxal_runtimes.channel;
    let platform_names = catalog.names_for_api(&api_version);
    if platform_names.is_empty() {
        bail!(
            "{}",
            format_unavailable_api_version_error(catalog, &api_version, channel.as_str())
        );
    }
    robot
        .validate_with(&platform_names)
        .map_err(|errors| anyhow!("Robot errors:\n{}", join_errors(errors)))?;

    let mut platform_runtimes = Vec::new();
    for entry in catalog.entries_for_api(&api_version) {
        let image_ref = robot
            .phoxal_runtimes
            .images
            .get(entry.name)
            .cloned()
            .unwrap_or_else(|| {
                format!(
                    "{}:{}-{}",
                    entry.image_repo(),
                    api_version,
                    channel.as_str()
                )
            });
        // Only attempt registry digest resolution when the flow asks for it
        // (`resolve_external_artifacts`, e.g. `deploy build`). Otherwise stay
        // unpinned and deploy by tag - we never synthesize a placeholder
        // `sha256:` that would later be mistaken for a real OCI pin.
        let pin = if let Some(digest) = image_ref_digest(&image_ref) {
            ImagePin::Digest(digest.to_string())
        } else if options.resolve_external_artifacts {
            ImagePin::Digest(resolve_image_digest(&image_ref)?)
        } else {
            ImagePin::Unpinned
        };
        platform_runtimes.push(ResolvedPlatformRuntime {
            name: entry.name.to_string(),
            image_ref,
            pin,
        });
    }

    let user_runtimes = robot
        .user_runtimes
        .iter()
        .map(|(name, runtime)| {
            resolve_user_runtime(
                project_root,
                &robot.identity.id,
                &api_version,
                name,
                runtime,
            )
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
        api_version,
        channel,
        platform_runtimes,
        user_runtimes,
        components,
        tools,
    })
}

/// Resolve a real OCI content digest for `image_ref` from the registry.
///
/// This requires `docker buildx imagetools inspect`, which reports the actual
/// registry index digest. We deliberately do **not** fall back to hashing a
/// `docker manifest inspect` body — fabricating a `sha256:` from manifest JSON
/// produces a string that looks like an OCI pin but can never be pulled. If
/// buildx cannot reach the image, fail loudly with guidance instead.
pub fn resolve_image_digest(image_ref: &str) -> Result<String> {
    let output = shell::run_stdout(
        "docker",
        [
            "buildx",
            "imagetools",
            "inspect",
            image_ref,
            "--format",
            "{{json .Manifest}}",
        ],
        None,
    )
    .with_context(|| {
        format!(
            "could not resolve a real image digest for {image_ref}. \
             `docker buildx imagetools inspect` failed — install Docker with buildx and ensure \
             the daemon can reach the registry. If the phoxal/framework GHCR runtime images are \
             not published yet, deploy by tag (`simulate`/`check` resolve no digests) until they \
             are published."
        )
    })?;
    buildx_imagetools_manifest_digest(&output).with_context(|| {
        format!("docker buildx imagetools inspect did not report an index digest for {image_ref}")
    })
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
    robot_id: &str,
    api_version: &str,
    name: &str,
    runtime: &UserRuntime,
) -> Result<ResolvedUserRuntime> {
    let runtime_dir = resolve_project_path(project_root, &runtime.path);
    if !runtime_dir.is_dir() {
        bail!(
            "user runtime '{name}' source dir {} does not exist; user runtimes must have an on-disk source directory to hash/build",
            runtime_dir.display()
        );
    }
    let framework = resolve_user_runtime_framework(name, &runtime.framework, api_version)?;
    let source_hash = hash_tree(&runtime_dir).with_context(|| {
        format!(
            "failed to hash user runtime '{name}' source tree at {}",
            runtime_dir.display()
        )
    })?;
    let image = format!("phoxal.local/{robot_id}/user-runtime/{name}:dev");
    let build = runtime
        .build
        .as_ref()
        .map(mirror_user_runtime_build)
        .transpose()?;

    Ok(ResolvedUserRuntime {
        name: name.to_string(),
        path: runtime.path.clone(),
        framework,
        build,
        source_hash,
        image,
    })
}

fn mirror_user_runtime_build(build: &impl Serialize) -> Result<ResolvedUserRuntimeBuild> {
    let value =
        serde_yaml::to_value(build).context("failed to serialize user runtime build recipe")?;
    serde_yaml::from_value(value).context("failed to mirror user runtime build recipe")
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
    api_version: &str,
) -> Result<()> {
    if selector == "match-platform" || selector == api_version {
        return Ok(());
    }
    bail!(
        "user runtime '{runtime_name}': framework '{selector}' must be \"match-platform\" or the graph api_version '{api_version}'"
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

fn image_ref_digest(image_ref: &str) -> Option<&str> {
    image_ref
        .rsplit_once('@')
        .map(|(_, digest)| digest)
        .filter(|digest| digest.starts_with("sha256:"))
}

fn buildx_imagetools_manifest_digest(output: &str) -> Result<String> {
    let value: Value =
        serde_json::from_str(output).context("docker buildx imagetools output was not JSON")?;
    value
        .as_object()
        .and_then(|object| object.get("digest").or_else(|| object.get("Digest")))
        .and_then(Value::as_str)
        .filter(|digest| digest.starts_with("sha256:"))
        .map(ToString::to_string)
        .ok_or_else(|| anyhow!("manifest index digest not found"))
}

/// Deterministic non-cryptographic stand-in hash for **tool asset** entries
/// during offline resolution. Never used for OCI image digests — image pins
/// are either a real registry digest or an honest tag ref (see [`ImagePin`]).
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

fn format_unavailable_api_version_error(
    catalog: &PlatformRuntimeCatalog,
    api_version: &str,
    channel: &str,
) -> String {
    let mut available = catalog
        .entries
        .iter()
        .flat_map(|entry| entry.api_versions.iter().copied())
        .collect::<Vec<_>>();
    available.sort_unstable();
    available.dedup();
    let suggested = available
        .iter()
        .rev()
        .copied()
        .find(|candidate| *candidate != api_version);

    let mut message = format!(
        "API version {api_version} is not available on channel {channel}: this CLI has no complete official runtime image set for that API version"
    );

    // List the full expected official runtime ref set for the requested
    // (api_version, channel), mirroring `format_missing_images_error` in
    // check.rs. The catalog cannot resolve a complete set for the unavailable
    // version, so the expected refs are every known official runtime name at
    // the requested api/channel.
    let mut expected_refs = catalog
        .names()
        .map(|name| format!("ghcr.io/phoxal/runtime-{name}:{api_version}-{channel}"))
        .collect::<Vec<_>>();
    expected_refs.sort_unstable();
    expected_refs.dedup();
    if !expected_refs.is_empty() {
        message.push_str("\n\nExpected official runtime images:");
        for image_ref in &expected_refs {
            message.push_str("\n  - ");
            message.push_str(image_ref);
        }
    }

    if !available.is_empty() {
        message.push_str("\n\nAvailable api_versions in this CLI: ");
        message.push_str(&available.join(", "));
    }
    message.push_str("\n\nFix:");
    if let Some(suggested) = suggested {
        message.push_str("\n  - use api_version: ");
        message.push_str(suggested);
    } else {
        message.push_str("\n  - use an api_version listed by `phoxal-cli version`");
    }
    message.push_str(
        "\n  - or use phoxal_runtimes.channel: edge if this API version is intentionally experimental",
    );
    message.push_str("\n  - or wait until Phoxal publishes the complete ");
    message.push_str(api_version);
    message.push('-');
    message.push_str(channel);
    message.push_str(" official runtime set");
    message
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::CATALOG;

    #[test]
    fn resolve_without_source_commits_leaves_git_component_commits_empty() -> anyhow::Result<()> {
        // Flows that never read component commits (`pull`, `outdated`) resolve
        // with `resolve_source_commits: false` and must NOT run `git ls-remote`.
        // A git component is resolved with an empty commit; if resolution tried
        // to reach the network it would either hang or fail, so an empty commit
        // proves no ls-remote was attempted.
        let robot = Robot::parse_from_string(GIT_COMPONENT_ROBOT)?;
        let resolved = resolve(
            &robot,
            std::path::Path::new("."),
            &CATALOG,
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
phoxal_runtimes:
  channel: stable
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
      git: https://github.com/phoxal/components
      tag: main
      directory: ddsm115
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
        let resolved = resolve(
            &robot,
            std::path::Path::new("."),
            &CATALOG,
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
    fn load_robot_tolerates_user_runtime_image_and_config() -> anyhow::Result<()> {
        // The typed `phoxal` model denies unknown fields and does not carry
        // `user_runtimes.<name>.image`/`config`; `load_robot` must strip them (like
        // the extras-aware loader) so every command — not just deploy/simulate —
        // accepts a manifest that declares them.
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
phoxal_runtimes:
  channel: stable
user_runtimes:
  brain:
    path: runtimes/brain
    image: ghcr.io/acme/brain@sha256:abc
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

        // Plain load_robot (used by check/validate/update/runtime add) must parse it
        // despite the image/config keys the typed model does not know about.
        let robot = load_robot(&path)?;
        assert!(robot.user_runtimes.contains_key("brain"));

        // The extras-aware loader still parses it too.
        let loaded = load_robot_with_extras(&path)?;
        assert!(loaded.robot.user_runtimes.contains_key("brain"));

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
phoxal_runtimes:
  channel: stable
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
phoxal_runtimes:
  channel: stable
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
phoxal_runtimes:
  channel: stable
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
phoxal_runtimes:
  channel: stable
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
    fn buildx_imagetools_digest_uses_index_digest_not_platform_leaf() -> anyhow::Result<()> {
        let output = r#"{
  "mediaType": "application/vnd.oci.image.index.v1+json",
  "digest": "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
  "manifests": [
    {
      "digest": "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
      "platform": {
        "architecture": "amd64",
        "os": "linux"
      }
    },
    {
      "digest": "sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
      "platform": {
        "architecture": "arm64",
        "os": "linux"
      }
    }
  ]
}"#;

        assert_eq!(
            buildx_imagetools_manifest_digest(output)?,
            "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        );

        Ok(())
    }

    #[test]
    fn buildx_imagetools_digest_rejects_non_sha256_digest() {
        // A registry that reports a non-sha256 digest must not be accepted as a
        // pin — we never coerce a bogus value into a `sha256:` string.
        let output = r#"{"digest": "md5:deadbeef"}"#;
        assert!(buildx_imagetools_manifest_digest(output).is_err());
    }

    fn runtime(pin: ImagePin) -> ResolvedPlatformRuntime {
        ResolvedPlatformRuntime {
            name: "asset".to_string(),
            image_ref: "ghcr.io/phoxal/runtime-asset:y2026_1-stable".to_string(),
            pin,
        }
    }

    #[test]
    fn unpinned_runtime_deploys_by_tag_not_fabricated_digest() {
        let runtime = runtime(ImagePin::Unpinned);
        assert_eq!(
            runtime.tag_ref(),
            "ghcr.io/phoxal/runtime-asset:y2026_1-stable"
        );
        // The deploy ref is the tag — no `@sha256:` is ever invented.
        assert_eq!(
            runtime.deploy_ref(),
            "ghcr.io/phoxal/runtime-asset:y2026_1-stable"
        );
        assert!(!runtime.deploy_ref().contains('@'));
        assert_eq!(runtime.digest_pin(), None);
    }

    #[test]
    fn pinned_runtime_deploys_by_digest() {
        let digest = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let runtime = runtime(ImagePin::Digest(digest.to_string()));
        assert_eq!(
            runtime.deploy_ref(),
            format!("ghcr.io/phoxal/runtime-asset:y2026_1-stable@{digest}")
        );
        assert_eq!(runtime.digest_pin(), Some(digest));
    }

    #[test]
    fn digest_override_deploys_as_the_override_ref_without_double_pin() {
        let digest = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let runtime = ResolvedPlatformRuntime {
            name: "asset".to_string(),
            image_ref: format!("ghcr.io/phoxal/runtime-asset@{digest}"),
            pin: ImagePin::Digest(digest.to_string()),
        };

        assert_eq!(runtime.deploy_ref(), runtime.image_ref);
        assert_eq!(runtime.digest_pin(), Some(digest));
    }
}
