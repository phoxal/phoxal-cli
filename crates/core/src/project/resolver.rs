//! Robot-manifest loading and terminal-independent resolution records.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use phoxal::model::robot::RobotV0 as Robot;

use super::suite::ArtifactKind;

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

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResolveOptions {
    /// Override the official service/driver target triple. Deploy probes the
    /// robot arch and resolves suite assets for that Linux triple instead of
    /// the host.
    pub official_target_triple: Option<String>,
    /// Override native tool asset target triple. Host-native run/sim use the
    /// host triple; deploy ships robot-native tools.
    pub tool_target_triple: Option<String>,
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
    /// diagnostics (`ensure_suite_availability`).
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
    Path {
        path: PathBuf,
    },
    /// Resolves from the official artifact suite (no fork pin for this
    /// package); staged from a suite release asset.
    Suite,
}

/// A resolved native artifact (`tool-bus`, `tool-log`, `tool-joypad`, or
/// `infrastructure-router`). `name` is the short,
/// launch-safe kind-qualified id used for participant ids, systemd unit
/// names and env var keys; `package` is the
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
    fn tool_emit_apis_id_strips_provider_and_tool_prefixes() {
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
}
