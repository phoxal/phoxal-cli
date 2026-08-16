//! Deployment-release candidate construction and validation.
//!
//! The live `.phoxal/release/` is replaced only after every canonical input,
//! every binary, and the release's own supervisor have passed validation, so
//! staging always builds into an adjacent `.phoxal/.release-candidate-<unique>`
//! directory first and publishes it with one atomic rename. A build that fails
//! halfway through must never leave a robot with no runtime, and a published
//! release always has both halves.
//!
//! The candidate's `bundle/` is exactly the shape the framework's bundle
//! reader reads. Authored documents are never staged: the compiled
//! `manifest.json` absorbs them, and only compiled assets ride along.
//!
//! ```text
//! phoxal-supervisor                       written last, by the release step
//! bundle/manifest.json                    phoxal/manifest/v0
//! bundle/assets/robot/meshes/...
//! bundle/assets/components/<type>/meshes/...
//! bundle/bin/brain
//! bundle/bin/<service-id>
//! bundle/bin/<component-type>
//! ```

use std::fs;
use std::path::{Path, PathBuf};

use crate::source::resolver::BundlePlan;
use anyhow::{Context, Result, ensure};
use phoxal_bundle::{ASSETS_DIR, BIN_DIR};

/// An unpublished deployment-release candidate.
pub(crate) struct StagedCandidate {
    pub(super) dir: tempfile::TempDir,
    pub(super) bundle: PathBuf,
    pub(super) project_root: PathBuf,
}

impl StagedCandidate {
    /// The bundle root being staged. Everything that materializes, checks, or
    /// validates runtime content works against this.
    #[must_use]
    pub(crate) fn path(&self) -> &Path {
        &self.bundle
    }

    /// The release root the bundle is staged inside. Only the release step,
    /// which adds the supervisor, works against this.
    #[must_use]
    pub(crate) fn release_path(&self) -> &Path {
        self.dir.path()
    }
}

pub(crate) fn begin_runtime_layout(
    project_root: &Path,
    resolved: &BundlePlan,
) -> Result<StagedCandidate> {
    let live = crate::paths::runtime::runtime_release_root(project_root);
    let parent = live
        .parent()
        .context("deployment release directory has no parent")?;
    fs::create_dir_all(parent).with_context(|| {
        format!(
            "failed to create runtime layout directory {}",
            parent.display()
        )
    })?;
    // The candidate is a sibling of the live release so publication is a rename
    // on one filesystem, never a copy.
    let candidate = tempfile::Builder::new()
        .prefix(".release-candidate-")
        .tempdir_in(parent)
        .with_context(|| {
            format!(
                "failed to create runtime layout candidate in {}",
                parent.display()
            )
        })?;

    let bundle = candidate.path().join(crate::deployment::BUNDLE_DIR);
    stage_candidate(&bundle, resolved)?;

    Ok(StagedCandidate {
        dir: candidate,
        bundle,
        project_root: project_root.to_path_buf(),
    })
}

fn stage_candidate(candidate: &Path, resolved: &BundlePlan) -> Result<()> {
    let asset_root = candidate.join(ASSETS_DIR);
    fs::create_dir_all(&asset_root)
        .with_context(|| format!("failed to create {}", asset_root.display()))?;

    // The canonical model's logical assets: robot and component meshes.
    for (id, bytes) in &resolved.compiled.assets {
        write_asset(&asset_root, id.as_str(), bytes)?;
    }

    fs::create_dir_all(candidate.join(BIN_DIR))
        .context("failed to create the bundle bin directory")?;
    Ok(())
}

fn write_asset(root: &Path, id: &str, bytes: &[u8]) -> Result<()> {
    validate_asset_path(id)?;
    let path = root.join(id);
    let parent = path.parent().context("bundle asset has no parent")?;
    fs::create_dir_all(parent).with_context(|| format!("failed to create {}", parent.display()))?;
    fs::write(&path, bytes).with_context(|| format!("failed to write {}", path.display()))
}

fn validate_asset_path(id: &str) -> Result<()> {
    ensure!(
        !id.is_empty()
            && Path::new(id)
                .components()
                .all(|component| matches!(component, std::path::Component::Normal(_))),
        "bundle asset '{id}' must contain only normal relative path components"
    );
    Ok(())
}

/// Copy a validated staged tree into a build destination.
pub(crate) fn copy_tree_into(source: &Path, dest: &Path) -> Result<()> {
    copy_dir_recursive(source, dest)
}

fn copy_dir_recursive(source: &Path, dest: &Path) -> Result<()> {
    fs::create_dir_all(dest).with_context(|| format!("failed to create {}", dest.display()))?;
    for entry in
        fs::read_dir(source).with_context(|| format!("failed to read {}", source.display()))?
    {
        let entry = entry?;
        let source_path = entry.path();
        let dest_path = dest.join(entry.file_name());
        let metadata = fs::symlink_metadata(&source_path)?;
        ensure!(
            !metadata.file_type().is_symlink(),
            "bundle content must not contain symlinks: {}",
            source_path.display()
        );
        if metadata.is_dir() {
            copy_dir_recursive(&source_path, &dest_path)?;
        } else if metadata.is_file() {
            fs::copy(&source_path, &dest_path).with_context(|| {
                format!(
                    "failed to stage runtime asset {} to {}",
                    source_path.display(),
                    dest_path.display()
                )
            })?;
        }
    }
    Ok(())
}

/// Compile a minimal real project into a [`CompiledBundle`], for fixtures that
/// need a plausible compiler output without staging a whole bundle.
#[cfg(test)]
pub(crate) fn compile_test_bundle(
    source_manifest: &phoxal_manifest::source::robot::v0::Manifest,
) -> Result<crate::source::resolver::CompiledBundle> {
    let source = tempfile::tempdir()?;
    let component = source.path().join("components/wheel");
    fs::create_dir_all(&component)?;
    fs::write(
        source.path().join("robot.yaml"),
        r#"schema: phoxal/robot/v0
robot:
  id: testbot
  motion_limits:
    max_linear_speed_mps: 0.6
    max_angular_speed_radps: 2.0
  structure: structure.urdf
  kinematic:
    kind: omnidirectional
    actuators:
      - wheel.motor
    encoders: []
  components:
    wheel:
      component: wheel
      mount_link: base_link
"#,
    )?;
    fs::write(
        source.path().join("structure.urdf"),
        r#"<robot name="testbot"><link name="base_footprint"/><link name="base_link"/><joint name="root" type="fixed"><parent link="base_footprint"/><child link="base_link"/></joint></robot>"#,
    )?;
    fs::write(
        component.join("component.yaml"),
        "schema: phoxal/component/v0\ncapabilities:\n  motor:\n    kind: motor\n    command: velocity\n    target:\n      kind: joint\n      id: wheel_joint\n",
    )?;
    fs::write(
        component.join("structure.urdf"),
        r#"<robot name="wheel"><link name="base"/><link name="wheel"/><joint name="wheel_joint" type="continuous"><parent link="base"/><child link="wheel"/></joint></robot>"#,
    )?;
    let mut fixture = crate::source::resolver::parse_robot_from_string(&fs::read_to_string(
        source.path().join("robot.yaml"),
    )?)?;
    fixture.services = source_manifest.services.clone();
    phoxal_manifest::source::robot::Manifest::V0(fixture).write_to_dir(source.path())?;
    let bundle = crate::source::resolver::CompiledBundle::from_project(
        phoxal_manifest::SourceSet {
            project_root: source.path().to_path_buf(),
            robot_manifest: source.path().join("robot.yaml"),
            component_roots: std::collections::BTreeMap::from([("wheel".to_string(), component)]),
        }
        .compile(std::iter::empty())?,
    );
    Ok(bundle)
}
