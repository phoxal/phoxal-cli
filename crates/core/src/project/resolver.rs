//! Robot-manifest loading and terminal-independent resolution records.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use phoxal_manifest::source::robot::v0::Manifest as Robot;
use phoxal_manifest::{AssetId, CompiledProject, Participant};

use super::catalog::ArtifactKind;

const ROBOT_FILE: &str = "robot.yaml";

pub fn official_binary_name(kind: ArtifactKind, name: &str) -> String {
    match kind {
        ArtifactKind::ComponentDriver => format!("phoxal-component-{name}"),
        ArtifactKind::Service | ArtifactKind::Simulator => {
            format!("phoxal-{kind}-{name}")
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolveOptions {
    /// Override the official service/driver target triple. `build --target`
    /// materializes for that triple instead of the host, so a robot graph can
    /// be cross-compiled from a non-Linux host.
    pub official_target_triple: Option<String>,
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
    /// Pass `--offline` to every Cargo/registry operation resolution makes.
    /// `PHOXAL_OFFLINE` is a Phoxal-only env var Cargo does not
    /// recognize, so this must be threaded explicitly from the caller's own
    /// `--offline`/`AppContext::offline`, not read back from the
    /// environment.
    pub offline: bool,
}

impl Default for ResolveOptions {
    fn default() -> Self {
        Self {
            official_target_triple: None,
            drivers: crate::project::layout::DriverSelection::default(),
            include_simulators: true,
            offline: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct BundlePlan {
    pub source_manifest: Robot,
    /// The single canonical compiler output consumed by staging, launch
    /// planning, and simulation. Source documents remain available only for
    /// package resolution and CLI-owned settings such as router policy.
    pub compiled: CompiledBundle,
    pub train: String,
    pub target: String,
    pub platform_runtimes: Vec<ResolvedPlatformRuntime>,
    pub simulators: Vec<ResolvedPlatformRuntime>,
    pub user_runtimes: Vec<ResolvedUserRuntime>,
    /// Workspace runtime crates present under `services/` but not declared in
    /// robot.yaml (and not official-identity overrides). They are
    /// not built or launched; graph validation and the staging summary
    /// surface them as drift diagnostics (#950).
    pub undeclared_runtimes: Vec<UndeclaredRuntime>,
    pub components: Vec<ResolvedComponent>,
    pub path_overrides: Vec<ResolvedPathOverride>,
}

/// Owned, comparison-friendly representation of [`CompiledProject`].
///
/// The framework keeps canonical model storage private. The CLI therefore
/// retains its deterministic wire encoding, normalized participant
/// declarations, and logical assets without inventing a parallel model.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct CompiledBundle {
    pub robot: Vec<u8>,
    pub participants: Vec<Participant>,
    pub assets: BTreeMap<AssetId, Vec<u8>>,
}

impl CompiledBundle {
    pub fn from_project(project: CompiledProject) -> Result<Self> {
        let (robot, participants, assets) = project.into_parts();
        Ok(Self {
            robot: robot
                .encode()
                .context("failed to encode the compiled canonical robot")?,
            participants: participants.into_vec(),
            assets: assets.into_map(),
        })
    }

    pub fn decode_robot(&self) -> Result<phoxal_model::Robot> {
        phoxal_model::Robot::decode(&self.robot)
            .context("failed to decode the compiled canonical robot")
    }
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
    /// "services" - the directory family and the robot.yaml map the
    /// crate would be declared in.
    pub family: &'static str,
}

/// One resolved `robot.components.<instance>` entry. Authored assets have one
/// direct source root; they are not a runtime artifact or Cargo-package half.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedComponent {
    pub instance: String,
    /// The logical component id (`component: <id>` in `robot.yaml`).
    pub source_name: String,
    /// Safe directory holding `component.yaml` and the authored assets.
    pub assets_root: PathBuf,
    /// The selected executable driver, if declared and included by policy.
    pub driver: Option<ResolvedComponentDriver>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolvedComponentDriver {
    Local { crate_dir: PathBuf },
    Registry(ResolvedPlatformRuntime),
}

impl ResolvedComponentDriver {
    #[must_use]
    pub fn source_path(&self) -> Option<&Path> {
        match self {
            Self::Local { crate_dir } => Some(crate_dir),
            Self::Registry(_) => None,
        }
    }

    #[must_use]
    pub fn registry_runtime(&self) -> Option<&ResolvedPlatformRuntime> {
        match self {
            Self::Local { .. } => None,
            Self::Registry(runtime) => Some(runtime),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolvedPathOverrideKind {
    Service,
    Simulator,
}

impl ResolvedPathOverrideKind {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Service => "service",
            Self::Simulator => "simulator",
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
    let robot = phoxal_manifest::source::robot::read_from_path(path)
        .with_context(|| format!("failed to read robot file {}", path.display()))?;
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
    fn official_binary_name_prefixes_by_artifact_kind() {
        assert_eq!(
            official_binary_name(ArtifactKind::Service, "drive"),
            "phoxal-service-drive"
        );
        assert_eq!(
            official_binary_name(ArtifactKind::ComponentDriver, "ddsm115"),
            "phoxal-component-ddsm115"
        );
        assert_eq!(
            official_binary_name(ArtifactKind::Simulator, "webots-controller"),
            "phoxal-simulator-webots-controller"
        );
    }
}
