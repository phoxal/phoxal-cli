//! Finalized-bundle candidate construction and validation.
//!
//! The live `.phoxal/bundle/` is replaced only after every canonical input and
//! every binary has passed validation, so staging always builds into an
//! adjacent `.phoxal/.bundle-candidate-<unique>` directory first and publishes
//! it with one atomic rename. A build that fails halfway through must never
//! leave a robot with no runtime.
//!
//! The candidate is exactly the shape the framework's finalized-bundle loader
//! reads:
//!
//! ```text
//! robot.yaml                              finalized robot/v0
//! assets/robot/structure.urdf
//! assets/robot/meshes/...
//! assets/components/<type>/component.yaml
//! assets/components/<type>/structure.urdf
//! assets/components/<type>/simulation.yaml   when the component has one
//! assets/components/<type>/meshes/...
//! assets/router/config.json5                 when the robot declares one
//! bin/...
//! ```

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use phoxal_cli_core::project::intent::RunIntent;
use phoxal_cli_core::project::layout::{ASSETS_DIR, BIN_DIR, ROBOT_FILE, ROUTER_CONFIG_ASSET};
use phoxal_cli_core::project::resolver::BundlePlan;

use super::finalize::{ROBOT_STRUCTURE_ASSET, finalize_manifest, render_finalized};

/// The frozen files a component definition contributes to a bundle, beside its
/// meshes. `simulation.yaml` is optional; the other two are not.
const COMPONENT_DEFINITION_FILES: [(&str, bool); 3] = [
    ("component.yaml", true),
    ("structure.urdf", true),
    ("simulation.yaml", false),
];

/// An unpublished bundle candidate.
pub(crate) struct StagedCandidate {
    pub(super) dir: tempfile::TempDir,
    pub(super) project_root: PathBuf,
}

impl StagedCandidate {
    #[must_use]
    pub(crate) fn path(&self) -> &Path {
        self.dir.path()
    }
}

pub(crate) fn begin_runtime_layout(
    project_root: &Path,
    resolved: &BundlePlan,
    intent: &RunIntent,
) -> Result<StagedCandidate> {
    let live =
        project_root.join(phoxal_cli_core::project::launch_plan::RUNTIME_BUNDLE_ROOT_RELATIVE);
    let parent = live
        .parent()
        .context("runtime bundle directory has no parent")?;
    fs::create_dir_all(parent).with_context(|| {
        format!(
            "failed to create runtime layout directory {}",
            parent.display()
        )
    })?;
    // The candidate is a sibling of the live bundle so publication is a rename
    // on one filesystem, never a copy.
    let candidate = tempfile::Builder::new()
        .prefix(".bundle-candidate-")
        .tempdir_in(parent)
        .with_context(|| {
            format!(
                "failed to create runtime layout candidate in {}",
                parent.display()
            )
        })?;

    stage_candidate(project_root, candidate.path(), resolved, intent)?;

    Ok(StagedCandidate {
        dir: candidate,
        project_root: project_root.to_path_buf(),
    })
}

fn stage_candidate(
    project_root: &Path,
    candidate: &Path,
    resolved: &BundlePlan,
    intent: &RunIntent,
) -> Result<()> {
    let finalized = finalize_manifest(&resolved.source_manifest, intent)?;
    fs::write(candidate.join(ROBOT_FILE), render_finalized(&finalized)?)
        .context("failed to write the finalized robot document")?;

    let asset_root = candidate.join(ASSETS_DIR);
    fs::create_dir_all(&asset_root)
        .with_context(|| format!("failed to create {}", asset_root.display()))?;

    // The canonical model's logical assets: robot and component meshes.
    for (id, bytes) in &resolved.compiled.assets {
        write_asset(&asset_root, id.as_str(), bytes)?;
    }

    // The robot structure, at its deterministic bundle-relative location.
    let structure = project_root.join(&resolved.source_manifest.robot.structure);
    let structure_bytes = fs::read(&structure)
        .with_context(|| format!("failed to read robot structure {}", structure.display()))?;
    write_asset(&asset_root, ROBOT_STRUCTURE_ASSET, &structure_bytes)?;

    // The frozen component definitions, keyed by component type so the loader
    // derives their roots deterministically.
    for component in &resolved.components {
        let destination = format!("components/{}", component.source_name);
        for (file, required) in COMPONENT_DEFINITION_FILES {
            let source = component.assets_root.join(file);
            if !source.is_file() {
                ensure!(
                    !required,
                    "component type '{}' is missing {file} at {}",
                    component.source_name,
                    component.assets_root.display()
                );
                continue;
            }
            let bytes = fs::read(&source)
                .with_context(|| format!("failed to read {}", source.display()))?;
            write_asset(&asset_root, &format!("{destination}/{file}"), &bytes)?;
        }
    }

    if let Some(source) = &resolved.source_manifest.router.config {
        ensure!(
            !source.as_os_str().is_empty()
                && source
                    .components()
                    .all(|component| matches!(component, std::path::Component::Normal(_))),
            "router.config must be a non-empty relative path without '.' or '..': {}",
            source.display()
        );
        let source = project_root.join(source);
        let bytes = fs::read(&source)
            .with_context(|| format!("failed to read router config {}", source.display()))?;
        write_asset(&asset_root, ROUTER_CONFIG_ASSET, &bytes)?;
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
