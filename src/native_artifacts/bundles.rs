//! Component asset-bundle staging into resolved robot roots.

use super::ArtifactStoreLock;
use anyhow::Context;
use anyhow::Result;
use std::fs;
use std::path::Path;

pub fn stage_component_bundles_into_robot_root(
    project_root: &Path,
    robot_root: &Path,
    resolved: &phoxal_cli_core::project::resolver::ResolvedRobot,
) -> Result<()> {
    let mut staged = std::collections::BTreeSet::new();
    let mut bundles = Vec::new();
    for component in &resolved.components {
        let component_id = &component.source_name;
        if !staged.insert(component_id.clone()) {
            continue;
        }
        let Some(source_dir) =
            crate::component_driver::component_assets_dir(component, project_root).with_context(
                || format!("failed to locate component assets for '{component_id}'"),
            )?
        else {
            // Driverless (passive) component with no official assets
            // package - nothing to stage.
            continue;
        };
        let dest_dir = robot_root.join("components").join(component_id);
        if source_dir == dest_dir {
            continue;
        }
        bundles.push((source_dir, dest_dir));
    }
    let _lock = crate::host_paths::artifacts_dir()
        .is_ok_and(|path| path.is_dir())
        .then(ArtifactStoreLock::shared)
        .transpose()?;
    for (source_dir, dest_dir) in bundles {
        copy_component_bundle_files(&source_dir, &dest_dir)?;
    }
    Ok(())
}

/// Copy one component's asset bundle files from `source_dir` into `dest_dir`.
/// `component.yaml` is required; `structure.urdf`/`simulation.yaml`/`meshes/`
/// are optional per component.
pub(crate) fn copy_component_bundle_files(source_dir: &Path, dest_dir: &Path) -> Result<()> {
    const COMPONENT_FILE: &str = "component.yaml";
    const OPTIONAL_FILES: [&str; 2] = ["structure.urdf", "simulation.yaml"];
    const MESHES_DIR: &str = "meshes";

    fs::create_dir_all(dest_dir)
        .with_context(|| format!("failed to create {}", dest_dir.display()))?;

    let component_file = source_dir.join(COMPONENT_FILE);
    fs::copy(&component_file, dest_dir.join(COMPONENT_FILE)).with_context(|| {
        format!(
            "failed to stage component metadata {} to {}",
            component_file.display(),
            dest_dir.display()
        )
    })?;

    for optional_file in OPTIONAL_FILES {
        let source_file = source_dir.join(optional_file);
        if !source_file.is_file() {
            continue;
        }
        fs::copy(&source_file, dest_dir.join(optional_file)).with_context(|| {
            format!(
                "failed to stage {} to {}",
                source_file.display(),
                dest_dir.display()
            )
        })?;
    }

    let meshes_source = source_dir.join(MESHES_DIR);
    if meshes_source.is_dir() {
        copy_dir_recursive_into(&meshes_source, &dest_dir.join(MESHES_DIR))?;
    }
    Ok(())
}

pub(crate) fn copy_dir_recursive_into(source: &Path, dest: &Path) -> Result<()> {
    fs::create_dir_all(dest).with_context(|| format!("failed to create {}", dest.display()))?;
    for entry in
        fs::read_dir(source).with_context(|| format!("failed to read {}", source.display()))?
    {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let source_path = entry.path();
        let dest_path = dest.join(entry.file_name());
        if file_type.is_dir() {
            copy_dir_recursive_into(&source_path, &dest_path)?;
        } else if file_type.is_file() {
            fs::copy(&source_path, &dest_path).with_context(|| {
                format!(
                    "failed to stage {} to {}",
                    source_path.display(),
                    dest_path.display()
                )
            })?;
        }
    }
    Ok(())
}
