//! Robot-manifest loading and terminal-independent resolution records.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use phoxal::model::robot::RobotV0 as Robot;

use super::catalog::ArtifactKind;

const PHOXAL_PROVIDER: &str = "phoxal";
const ROBOT_FILE: &str = "robot.yaml";

pub fn tool_participant_id(tool_name: &str) -> &str {
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
    /// Override the official service/driver target triple. `build --target`
    /// materializes for that triple instead of the host, so a robot graph can
    /// be cross-compiled from a non-Linux host.
    pub official_target_triple: Option<String>,
    /// Override native tool asset target triple. Host-native run/sim use the
    /// host triple; an explicit target resolves robot-native tools instead.
    pub tool_target_triple: Option<String>,
    /// The component-driver instances resolution may resolve driver binaries
    /// for. `run`'s driver policy threads through here so an excluded driver is
    /// never resolved - not even to select its target artifact (#936).
    /// Everything except driver-filtered resident staging resolves `All`.
    pub drivers: crate::project::layout::DriverSelection,
    /// Whether simulator-only artifacts belong to this resolution.
    ///
    /// Host run/check/simulation paths keep them. A native runtime bundle does
    /// not: simulators execute beside Webots on an operator host and are never
    /// installed on the robot target.
    pub include_simulators: bool,
}

impl Default for ResolveOptions {
    fn default() -> Self {
        Self {
            official_target_triple: None,
            tool_target_triple: None,
            drivers: crate::project::layout::DriverSelection::default(),
            include_simulators: true,
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
    /// The declared additional user tools (`tools:` in robot.yaml) resolved to
    /// their workspace crates (#950) - the tool analogue of `user_runtimes`.
    pub user_tools: Vec<ResolvedUserRuntime>,
    /// Workspace runtime crates present under `services/`/`tools/` but not
    /// declared in robot.yaml (and not official-identity overrides). They are
    /// not built or launched; graph validation and the staging summary
    /// surface them as drift diagnostics (#950).
    pub undeclared_runtimes: Vec<UndeclaredRuntime>,
    pub components: Vec<ResolvedComponent>,
    pub tools: Vec<ResolvedTool>,
    pub path_overrides: Vec<ResolvedPathOverride>,
}

/// One resolved official platform runtime (a service or a simulator). The
/// public identity is the provider-qualified `package` id
/// (`phoxal/service-drive`); there is no separate `artifact_id` (docs #21).
///
/// The official catalog (organization#951 WS4) is CLI-internal, so location
/// and integrity are Cargo's job: `package` at exactly `train` from the
/// `phoxal` registry, materialized by `cargo install`. Contract/config
/// metadata is always extracted from the materialized binary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedPlatformRuntime {
    pub name: String,
    pub package: String,
    pub kind: ArtifactKind,
    pub path_override: Option<PathBuf>,
    /// Exact locked framework train this entry belongs to; every official
    /// package is pinned to it exactly (`=<train>`).
    pub train: String,
    /// The target triple this entry was resolved/materialized for. `None`
    /// identifies the distinct component-assets scope.
    pub target: Option<String>,
}

impl ResolvedPlatformRuntime {
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

/// One workspace runtime crate that is present but not declared in robot.yaml
/// (#950): legal, not built, surfaced as a drift diagnostic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UndeclaredRuntime {
    /// The crate's logical name (its directory name).
    pub name: String,
    /// "services" or "tools" - the directory family and the robot.yaml map the
    /// crate would be declared in.
    pub family: &'static str,
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
    /// The resolved `component_assets` package: the workspace assets crate
    /// for a workspace component, or the official `phoxal/component-<id>`
    /// assets package. Every component resolves its assets - a driverless
    /// workspace component is an assets crate, and a component absent from
    /// both workspace and catalog is a resolution error, never a silent
    /// "assetless" (#936).
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
    /// The provider-qualified package id (`phoxal/component-ddsm115`).
    pub package: String,
    pub kind: ArtifactKind,
    pub source: ResolvedComponentSource,
    /// The on-disk directory this package resolves to, whichever source it
    /// came from: the workspace crate directory for `Path`, or the
    /// registry-package extraction directory `cargo metadata` reported for
    /// `Registry` (resolved once, at resolution time, against the generated
    /// `.phoxal/resolve/Cargo.toml`). `None` only for a `Registry` driver
    /// package - a driver has no directory to read; it materializes straight
    /// to a `bin/` binary via `cargo install`, exactly like a service.
    pub resolved_dir: Option<PathBuf>,
    /// Present exactly when `source == Registry`, carrying the identity a
    /// service/simulator resolves to so a driver package materializes
    /// through the identical `cargo install` path instead of a parallel
    /// bespoke one.
    pub registry_runtime: Option<ResolvedPlatformRuntime>,
}

impl ResolvedComponentPackage {
    #[must_use]
    pub fn path_override(&self) -> Option<&Path> {
        self.resolved_dir.as_deref()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolvedComponentSource {
    Path {
        path: PathBuf,
    },
    /// Resolves from the official Cargo registry (no fork pin for this
    /// package).
    Registry,
}

/// A resolved native artifact (`tool-bus`, `tool-log`, `tool-joypad`, or
/// `infrastructure-router`). `name` is the short,
/// launch-safe kind-qualified id used for participant ids, systemd unit
/// names and env var keys; `package` is the
/// canonical provider-qualified identity (`phoxal/tool-bus`) used for
/// catalog lookups and `cargo install` materialization.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedTool {
    pub kind: ArtifactKind,
    pub name: String,
    pub package: String,
    pub binary_name: String,
    pub path_override: Option<PathBuf>,
    /// Exact locked framework train this entry belongs to.
    pub train: String,
    /// The target triple this entry was resolved/materialized for.
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

#[derive(Debug, Clone, Eq, PartialEq)]
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
    crate::schema::ensure_supported_revision(path, crate::schema::DocumentKind::Robot)?;
    let robot = phoxal::model::robot::Robot::read_from_path(path)
        .with_context(|| format!("failed to read robot file {}", path.display()))?
        .into_v0();
    validate_launch_participant_ids(&robot, path)?;
    Ok(robot)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_participant_id_strips_provider_and_tool_prefixes() {
        assert_eq!(tool_participant_id("phoxal/tool-router"), "router");
        assert_eq!(tool_participant_id("tool-router"), "router");
        assert_eq!(tool_participant_id("router"), "router");
        assert_eq!(
            tool_participant_id("phoxal/infrastructure-router"),
            "router"
        );
        assert_eq!(tool_participant_id("infrastructure-router"), "router");
    }

    #[test]
    fn official_binary_name_uses_component_crate_binary_for_component_driver() {
        assert_eq!(
            official_binary_name(ArtifactKind::ComponentDriver, "ddsm115"),
            "phoxal-component-ddsm115"
        );
    }

    #[test]
    fn load_robot_gates_an_unsupported_schema_revision_before_parsing() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join(ROBOT_FILE);
        std::fs::write(
            &path,
            "schema: robot/v1\nrobot:\n  id: rover\n  namespace: dev\n",
        )?;
        let error = load_robot(&path).expect_err("robot/v1 must be gated");
        let message = format!("{error:#}");
        assert!(message.contains("robot/v1"), "{message}");
        assert!(message.contains("Update phoxal-cli"), "{message}");
        assert!(!message.contains("unknown variant"), "{message}");
        Ok(())
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
            official_binary_name(ArtifactKind::Simulator, "webots-controller"),
            "phoxal-simulator-webots-controller"
        );
    }
}
