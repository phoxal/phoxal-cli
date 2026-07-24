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
        // Schema gate every referenced component document on the native path,
        // exactly as the simulation stager already does (#936, finding 5):
        // check the declared `component/vX` revision before strict parsing, then
        // strict-parse it so an unknown field or unsupported revision fails here -
        // naming the exact file - instead of being copied through silently.
        gate_component_document(&source_dir, component_id)?;
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

/// Schema-gate one referenced component document before it is staged: verify the
/// declared `component/vX` revision this CLI supports, then strict-parse the
/// component from its directory so an unknown field is rejected too. Both errors
/// name the exact `component.yaml`. This is the native-path counterpart of the
/// gate `src/simulation/staging.rs` already applies (#936, finding 5).
fn gate_component_document(source_dir: &Path, component_id: &str) -> Result<()> {
    let component_file = source_dir.join("component.yaml");
    phoxal_cli_core::schema::ensure_supported_revision(
        &component_file,
        phoxal_cli_core::schema::DocumentKind::Component,
    )?;
    phoxal::model::component::Component::read_from_dir(source_dir).with_context(|| {
        format!(
            "component '{component_id}' failed strict parsing of {}",
            component_file.display()
        )
    })?;
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

#[cfg(test)]
mod tests {
    use super::*;

    /// An unsupported `component/vX` revision on the native staging path fails
    /// the schema gate with the precise "update the CLI" message, naming the
    /// exact file - it is never copied through silently (#936, finding 5).
    #[test]
    fn native_staging_rejects_an_unsupported_component_revision() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("component.yaml"), "schema: component/v3\n").unwrap();

        let error = gate_component_document(dir.path(), "ddsm115")
            .expect_err("an unsupported component revision must be gated")
            .to_string();
        assert!(error.contains("component/v3"), "{error}");
        assert!(error.contains("component.yaml"), "{error}");
    }

    /// An unknown field inside a supported revision on the native path is a hard
    /// strict-parse failure naming the component and file - the native stager no
    /// longer copies unvalidated `component.yaml` bytes (#936, finding 5).
    #[test]
    fn native_staging_rejects_an_unknown_component_field() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("component.yaml"),
            "schema: component/v0\nbogus_field: true\n",
        )
        .unwrap();

        let error = format!(
            "{:#}",
            gate_component_document(dir.path(), "ddsm115")
                .expect_err("an unknown component field must be rejected")
        );
        assert!(error.contains("ddsm115"), "{error}");
        assert!(error.contains("component.yaml"), "{error}");
    }
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
