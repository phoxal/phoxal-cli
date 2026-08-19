//! Authored-manifest loading and project resolution records.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use phoxal::authoring::CompiledProject;
use phoxal::authoring::source::robot::v0::Manifest as Robot;
use phoxal::model::AssetId;

use phoxal_cli_catalog::ArtifactKind;

const ROBOT_FILE: &str = "robot.yaml";

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResolveOptions {
    /// Override the official service/driver target triple. `build --target`
    /// materializes for that triple instead of the host, so a robot graph can
    /// be cross-compiled from a non-Linux host.
    pub official_target_triple: Option<String>,
    /// Pass `--offline` to every Cargo/registry operation resolution makes.
    /// `PHOXAL_OFFLINE` is a Phoxal-only env var Cargo does not
    /// recognize, so this must be threaded explicitly from the caller's own
    /// `--offline`/`AppContext::offline`, not read back from the
    /// environment.
    pub offline: bool,
}

#[derive(Debug, Clone)]
pub struct BundlePlan {
    pub source_manifest: Robot,
    /// The single canonical compiler output consumed by staging, launch
    /// planning, and simulation. Source documents remain available only for
    /// package resolution and CLI-owned settings such as router policy.
    pub compiled: CompiledBundle,
    /// The framework this project selected, read from its committed Cargo
    /// graph. It is the authority every participant binary in the plan is
    /// validated against, and the exact version official packages are pinned
    /// to.
    pub train: crate::source::train::LockedTrain,
    pub target: String,
    /// The one mandatory root brain, discovered from the root Cargo package.
    /// It is never optional or registry-resolved: every
    /// supported source project has exactly one.
    pub brain: ResolvedBrain,
    pub platform_runtimes: Vec<ResolvedPlatformRuntime>,
    pub user_runtimes: Vec<ResolvedUserRuntime>,
    /// Workspace runtime crates present under `services/` but not declared in
    /// robot.yaml (and not official-identity overrides). They are
    /// not built or launched; graph validation and the staging summary
    /// surface them as drift diagnostics.
    pub undeclared_runtimes: Vec<UndeclaredRuntime>,
    pub components: Vec<ResolvedComponent>,
    pub path_overrides: Vec<ResolvedPathOverride>,
}

/// The compiler output the CLI keeps in memory while it finalizes a bundle.
///
/// There is nothing beside the robot and its assets. The service list, the
/// driver list and the router configuration that used to sit here are all
/// inside the robot now - a service is a `services` entry, a driver is a
/// `components` entry whose `driver` block is present - so staging reads the
/// process set off the one document it is about to write.
#[derive(Debug, Clone)]
pub struct CompiledBundle {
    pub robot: phoxal::model::Robot,
    pub assets: BTreeMap<AssetId, Vec<u8>>,
}

impl CompiledBundle {
    #[must_use]
    pub fn from_project(project: CompiledProject) -> Self {
        let (robot, assets) = project.into_parts();
        Self {
            robot,
            assets: assets.into_map(),
        }
    }
}

/// One resolved official platform runtime (a service or a simulator). The
/// public identity is the provider-qualified `package` id
/// (`phoxal/service-drive`); there is no separate artifact identifier.
///
/// The official catalog is CLI-internal, so location
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

/// The robot project's root Cargo package, resolved as its one mandatory
/// brain source.
///
/// The canonical runtime identity is always `brain`; the Cargo package name
/// and binary target stay separate, project-specific facts so staging can
/// rename the verified executable to `bin/brain` without any of them leaking
/// into the launch graph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedBrain {
    /// The root Cargo package's own directory (the project root).
    pub crate_dir: PathBuf,
    /// The root Cargo package name, for diagnostics and container package
    /// selection.
    pub package: String,
    /// The exact Cargo-metadata-reported binary target the root package
    /// builds. Never inferred from `[[bin]]`, package naming, or directory
    /// naming.
    pub bin_target: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedUserRuntime {
    pub name: String,
    pub path: PathBuf,
}

/// One workspace `services/` crate that is present but not declared in
/// robot.yaml. It is legal, not built, and surfaced as a drift diagnostic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UndeclaredRuntime {
    /// The crate's logical name (its directory name).
    pub name: String,
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

/// One official service whose implementation a workspace `services/` crate
/// replaces. Only services can be overridden this way: a component driver is
/// selected by its component instance, not by directory discovery.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct ResolvedPathOverride {
    pub key: String,
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

/// Unwrap the versioned authored `robot.yaml` document to its exact body.
///
/// The schema tag selects the variant, so the CLI keeps one
/// projection point rather than destructuring the versioned enum at every
/// call site.
#[must_use]
pub fn robot_manifest_body(manifest: phoxal::authoring::source::robot::Manifest) -> Robot {
    let phoxal::authoring::source::robot::Manifest::V0(body) = manifest;
    body
}

/// Parse authored `robot.yaml` text into its exact body.
pub fn parse_robot_from_string(text: &str) -> Result<Robot> {
    Ok(robot_manifest_body(
        phoxal::authoring::source::robot::Manifest::parse(text)?,
    ))
}

/// Write an authored `robot.yaml` body back out under its versioned schema
/// tag.
pub fn write_robot_to_dir(robot: &Robot, dir: impl AsRef<Path>) -> Result<()> {
    phoxal::authoring::source::robot::Manifest::V0(robot.clone()).write_to_dir(dir)?;
    Ok(())
}

pub fn load_robot(path: &Path) -> Result<Robot> {
    crate::schema::ensure_supported_revision(path, crate::schema::DocumentKind::Robot)?;
    let robot = robot_manifest_body(
        phoxal::authoring::source::robot::Manifest::load(path)
            .with_context(|| format!("failed to read robot file {}", path.display()))?,
    );
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
    fn load_robot_gates_an_unsupported_schema_revision_before_parsing() -> Result<()> {
        let dir = tempfile::tempdir()?;

        let newer = dir.path().join(ROBOT_FILE);
        std::fs::write(&newer, "schema: phoxal/robot/v1\nrobot:\n  id: rover\n")?;
        let message = format!(
            "{:#}",
            load_robot(&newer).expect_err("phoxal/robot/v1 must be gated")
        );
        assert!(message.contains("phoxal/robot/v1"), "{message}");
        assert!(message.contains("Update the phoxal CLI"), "{message}");
        assert!(!message.contains("unknown variant"), "{message}");

        Ok(())
    }
}
