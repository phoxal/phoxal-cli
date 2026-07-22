//! Robot-manifest loading and terminal-independent resolution records.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use phoxal::model::robot::RobotV0 as Robot;
use serde_json::Value;

use super::suite::ArtifactKind;
use super::tooling::resolve_project_path;

const PHOXAL_PROVIDER: &str = "phoxal";
const ROBOT_FILE: &str = "robot.yaml";

pub fn tool_emit_apis_id(tool_name: &str) -> &str {
    tool_name
        .strip_prefix("phoxal/tool-")
        .or_else(|| tool_name.strip_prefix("phoxal/infrastructure-"))
        .or_else(|| tool_name.strip_prefix("tool-"))
        .or_else(|| tool_name.strip_prefix("infrastructure-"))
        .unwrap_or(tool_name)
}

pub fn official_binary_name(kind: ArtifactKind, name: &str) -> String {
    match kind {
        ArtifactKind::ComponentDriver => format!("phoxal-component-{name}"),
        ArtifactKind::ComponentAssets => {
            unreachable!("component_assets has no runtime binary to name")
        }
        ArtifactKind::Service
        | ArtifactKind::Tool
        | ArtifactKind::Simulator
        | ArtifactKind::Infrastructure => format!("phoxal-{kind}-{name}"),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolveOptions {
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
    /// robot arch and resolves suite assets for that Linux triple instead of
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
    pub train: String,
    pub target: String,
    pub platform_runtimes: Vec<ResolvedPlatformRuntime>,
    pub simulators: Vec<ResolvedPlatformRuntime>,
    pub user_runtimes: Vec<ResolvedUserRuntime>,
    pub components: Vec<ResolvedComponent>,
    pub tools: Vec<ResolvedTool>,
    pub path_overrides: Vec<ResolvedPathOverride>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct RobotManifestExtras {
    pub suite_source: Option<PathBuf>,
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
/// Location and integrity come from the suite. Contract/config metadata is
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
    /// Whether the suite has a built [`phoxal_cli_core::project::suite::Artifact`] (tarball)
    /// for the resolved target triple. `false` for a metadata-only / not yet
    /// published entry - resolution still succeeds (the package is real and
    /// versioned), but there is nothing to fetch yet.
    pub published: bool,
    /// Every target triple the suite has a built tarball for, for
    /// diagnostics (`ensure_suite_availability`, `generations status`).
    pub published_triples: Vec<String>,
    pub path_override: Option<PathBuf>,
    /// Exact locked framework train this entry belongs to.
    pub train: String,
    /// The target triple this entry was resolved/built for. `None` identifies
    /// the suite's distinct component-assets blob.
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
    /// the suite; that's a valid configuration, not an error. A
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
    /// Present exactly when `source == Suite` and the suite resolved a
    /// matching entry for the needed scope (assets or `context.target`).
    /// Carries the same shape a
    /// service/simulator resolves to ([`ResolvedPlatformRuntime`]) so
    /// components stage through the identical native-artifact machinery
    /// (`native_artifacts::NativeArtifactDescriptor`) instead of a parallel
    /// bespoke path. `None` for `Path`/`Git` sources.
    pub suite_runtime: Option<ResolvedPlatformRuntime>,
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
    /// Resolves from the official artifact suite (no fork pin for this
    /// package); staged from a suite release asset.
    Suite,
}

/// A resolved native artifact (`tool-bus`, `tool-log`, `tool-joypad`, or
/// `infrastructure-router`). `name` is the short,
/// launch-safe kind-qualified id used for participant/site ids, systemd unit
/// names, and env var keys (`ROBOT_TOOL_BUS` etc.); `package` is the
/// canonical provider-qualified identity (`phoxal/tool-bus`) used for
/// suite lookups and native-artifact provisioning.
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
    /// Whether the suite has a built [`phoxal_cli_core::project::suite::Artifact`] (tarball)
    /// for the resolved target triple; `false` for a metadata-only / not yet
    /// published entry, in which case `sha256` is a placeholder
    /// (`"0".repeat(64)`) rather than a real digest - mirrors
    /// [`ResolvedPlatformRuntime::published`].
    pub published: bool,
    pub path_override: Option<PathBuf>,
    /// Exact locked framework train this entry belongs to.
    pub train: String,
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
    // suite/release based.
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

pub fn is_launch_id(value: &str) -> bool {
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
        && let Some(suite) = artifacts.remove("suite")
    {
        extras.suite_source = Some(parse_suite_source_extra(&suite, robot_path)?);
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

fn parse_suite_source_extra(value: &serde_yaml::Value, robot_path: &Path) -> Result<PathBuf> {
    let Some(source) = value.as_str() else {
        bail!(
            "artifacts.suite in {} must be a local path string",
            robot_path.display()
        );
    };
    if source.trim().is_empty() {
        bail!(
            "artifacts.suite in {} must not be empty",
            robot_path.display()
        );
    }
    Ok(PathBuf::from(source))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

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
        assert_eq!(
            official_binary_name(ArtifactKind::ComponentDriver, "ddsm115"),
            "phoxal-component-ddsm115"
        );
    }

    #[test]
    fn official_binary_name_uses_suite_kind_for_other_kinds() {
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

    #[test]
    fn in_project_path_pins_allowed_in_base_but_escaping_ones_rejected() {
        let base = |pin: &str| {
            serde_yaml::from_str::<serde_yaml::Value>(&format!(
                "artifacts:\n  pins:\n    phoxal/component-local:\n      path: {pin}\n"
            ))
            .expect("test manifest should parse")
        };
        let manifest = Path::new("/proj/robot.yaml");
        for allowed in ["./components/passive_caster", "components/x", "a/b/../c"] {
            assert!(ensure_no_base_path_pins(&base(allowed), manifest).is_ok());
        }
        for escaping in ["../framework/service/drive", "/abs/path", "../../x"] {
            assert!(ensure_no_base_path_pins(&base(escaping), manifest).is_err());
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
        std::fs::create_dir_all(&local)?;
        std::fs::create_dir_all(&outside)?;
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
