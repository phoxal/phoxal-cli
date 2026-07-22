//! Robot metadata and component-asset staging.

use super::{
    ACTIVE_ROOT, COMPONENT_FILE, MESHES_DIR, OPT_ROOT, SIMULATION_FILE, STRUCTURE_FILE,
    payload_opt, write_text,
};
use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use phoxal::model::robot::v0::Robot;
use phoxal_cli_core::project::resolver::ResolvedComponentSource;
use phoxal_cli_core::project::resolver::ResolvedRobot;
use phoxal_cli_core::project::tooling::resolve_project_path;
use std::collections::BTreeSet;
use std::fs;
use std::path::Component as PathComponent;
use std::path::Path;
use std::path::PathBuf;

pub(crate) fn write_robot_yaml(root: &Path, robot: &Robot) -> Result<()> {
    // Serialize through the version dispatcher so the payload keeps the
    // `schema: robot/v0` tag - on-target consumers (tool-router, participants)
    // parse via the dispatcher and reject an untagged manifest.
    let yaml = serde_yaml::to_string(&phoxal::model::robot::Robot::V0(robot.clone()))
        .context("failed to serialize resolved robot.yaml")?;
    write_text(&payload_opt(root).join("robot.yaml"), &yaml)
}

/// Stage the robot's own structure file plus every resolved component's full
/// `component_assets` bundle (`component.yaml`, `simulation.yaml`,
/// `structure.urdf`, `meshes/`) into the deploy payload's
/// `/opt/phoxal/components/<component-id>/` (docs #21's deploy install
/// payload shape). One component id may back several instances; its bundle is
/// staged once and shared. Asset resolution never depends on where a
/// component checkout happened to live - only the resolved
/// `component_assets` package's own source directory matters.
///
/// `Path`-sourced (dev-overlay-pinned) assets stage from their local checkout
/// directly. `Suite`-sourced assets stage from the CLI's native-artifact
/// cache ONLY - never the network, matching the pre-existing offline
/// guarantee official service/tool binaries already have (`phoxal update`
/// populates the cache; deploy, live or `--dry-run`, only ever reads it). A
/// `Git`-sourced assets package is not staged here (no local checkout in the
/// deploy flow) - unchanged pre-existing scope.
pub(crate) fn stage_payload_metadata(
    project_root: &Path,
    root: &Path,
    robot: &Robot,
    resolved: &ResolvedRobot,
) -> Result<Vec<String>> {
    let reads_suite_assets = resolved.components.iter().any(|component| {
        component
            .assets
            .as_ref()
            .is_some_and(|assets| matches!(&assets.source, ResolvedComponentSource::Suite))
    });
    let _artifact_lock = (reads_suite_assets
        && crate::host_paths::artifacts_dir().is_ok_and(|path| path.is_dir()))
    .then(crate::native_artifacts::ArtifactStoreLock::shared)
    .transpose()?;
    let mut staged_files = Vec::new();
    staged_files.push(stage_declared_metadata_file(
        project_root,
        &payload_opt(root),
        &robot.robot.structure,
        "robot structure",
    )?);

    let mut staged_components = BTreeSet::new();
    for component in &resolved.components {
        let component_id = &component.source_name;
        // A driverless (passive) component with no official assets package
        // has nothing to stage here - `component.assets` is `None`.
        let source_dir = match component.assets.as_ref() {
            None => None,
            Some(assets) => match &assets.source {
                ResolvedComponentSource::Path { .. } => {
                    crate::component_driver::component_assets_dir(component, project_root)?
                }
                ResolvedComponentSource::Suite => locate_cached_component_assets_dir(assets)?,
                ResolvedComponentSource::Git { .. } => None,
            },
        };
        let Some(source_dir) = source_dir else {
            continue;
        };
        if !staged_components.insert(component_id.clone()) {
            continue;
        }
        let payload_dir = payload_components_dir(root).join(component_id);
        staged_files.extend(stage_component_assets_bundle(
            root,
            &source_dir,
            &payload_dir,
            component_id,
        )?);
    }

    Ok(staged_files)
}

/// Locate a Suite-sourced `component_assets` package's already-staged cache
/// directory, WITHOUT downloading - deploy (live or `--dry-run`) must never
/// reach the network for artifacts; `phoxal update` populates this
/// cache. `Ok(None)` when nothing is cached yet (a fresh clean machine that
/// has not run `update`) - the caller skips staging that component's assets
/// rather than erroring, since deploy's live-artifact `NativePending` gate
/// (`stage_official_artifacts`) is the authoritative "not available locally"
/// diagnostic surface; a missing assets-only bundle does not block a
/// hardware-driver deploy.
pub(crate) fn locate_cached_component_assets_dir(
    package: &phoxal_cli_core::project::resolver::ResolvedComponentPackage,
) -> Result<Option<PathBuf>> {
    let Some(runtime) = &package.suite_runtime else {
        return Ok(None);
    };
    let Some(descriptor) =
        phoxal_cli_core::artifacts::NativeArtifactDescriptor::from_runtime(runtime)?
    else {
        return Ok(None);
    };
    let exec_dir = crate::native_artifacts::artifact_exec_dir(&descriptor)?;
    Ok(exec_dir.is_dir().then_some(exec_dir))
}

pub(crate) fn payload_components_dir(root: &Path) -> PathBuf {
    payload_opt(root).join("components")
}

/// Copy one component's asset bundle (`component.yaml`, `simulation.yaml`,
/// `structure.urdf`, `meshes/`) from its resolved source directory into the
/// staged payload directory, returning the `/opt/phoxal/...`-rooted remote
/// paths of every file staged. Only files that actually exist are copied -
/// `simulation.yaml` and `meshes/` are optional per component.
pub(crate) fn stage_component_assets_bundle(
    root: &Path,
    source_dir: &Path,
    payload_dir: &Path,
    component_id: &str,
) -> Result<Vec<String>> {
    let mut staged_files = Vec::new();
    let component_file = source_dir.join(COMPONENT_FILE);
    copy_metadata_file(
        &component_file,
        &payload_dir.join(COMPONENT_FILE),
        &format!("component metadata for '{component_id}'"),
    )?;
    staged_files.push(payload_remote_path(root, &payload_dir.join(COMPONENT_FILE)));

    for optional_file in [STRUCTURE_FILE, SIMULATION_FILE] {
        let source_file = source_dir.join(optional_file);
        if !source_file.is_file() {
            continue;
        }
        copy_metadata_file(
            &source_file,
            &payload_dir.join(optional_file),
            &format!("component {optional_file} for '{component_id}'"),
        )?;
        staged_files.push(payload_remote_path(root, &payload_dir.join(optional_file)));
    }

    let meshes_source = source_dir.join(MESHES_DIR);
    if meshes_source.is_dir() {
        let meshes_dest = payload_dir.join(MESHES_DIR);
        copy_dir_recursive(root, &meshes_source, &meshes_dest, &mut staged_files)?;
    }

    Ok(staged_files)
}

/// The remote path a staged payload file is reported under, computed by
/// stripping the temp payload root (`payload_opt(root)`) and re-rooting under
/// `{OPT_ROOT}`.
pub(crate) fn payload_remote_path(root: &Path, payload_path: &Path) -> String {
    let relative = payload_path
        .strip_prefix(payload_opt(root))
        .unwrap_or(payload_path);
    opt_remote_path(relative)
}

pub(crate) fn copy_dir_recursive(
    root: &Path,
    source: &Path,
    dest: &Path,
    staged_files: &mut Vec<String>,
) -> Result<()> {
    fs::create_dir_all(dest).with_context(|| format!("failed to create {}", dest.display()))?;
    for entry in
        fs::read_dir(source).with_context(|| format!("failed to read {}", source.display()))?
    {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let source_path = entry.path();
        let dest_path = dest.join(entry.file_name());
        if file_type.is_dir() {
            copy_dir_recursive(root, &source_path, &dest_path, staged_files)?;
        } else if file_type.is_file() {
            fs::copy(&source_path, &dest_path).with_context(|| {
                format!(
                    "failed to stage {} to {}",
                    source_path.display(),
                    dest_path.display()
                )
            })?;
            staged_files.push(payload_remote_path(root, &dest_path));
        }
    }
    Ok(())
}

pub(crate) fn stage_declared_metadata_file(
    project_root: &Path,
    payload_root: &Path,
    relative_path: &Path,
    label: &str,
) -> Result<String> {
    validate_payload_relative_path(relative_path, label)?;
    let source = resolve_project_path(project_root, relative_path);
    copy_metadata_file(&source, &payload_root.join(relative_path), label)?;
    Ok(opt_remote_path(relative_path))
}

pub(crate) fn validate_payload_relative_path(path: &Path, field: &str) -> Result<()> {
    if path.as_os_str().is_empty() {
        bail!("{field} must not be empty");
    }
    if path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                PathComponent::ParentDir | PathComponent::RootDir | PathComponent::Prefix(_)
            )
        })
    {
        bail!(
            "{field} path '{}' must stay within {OPT_ROOT}",
            path.display()
        );
    }
    Ok(())
}

pub(crate) fn copy_metadata_file(source: &Path, dest: &Path, label: &str) -> Result<()> {
    if !source.is_file() {
        bail!("{label} file {} does not exist", source.display());
    }
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    fs::copy(source, dest).with_context(|| {
        format!(
            "failed to stage {label} {} to {}",
            source.display(),
            dest.display()
        )
    })?;
    Ok(())
}

pub(crate) fn opt_remote_path(relative_path: &Path) -> String {
    format!("{ACTIVE_ROOT}/{}", relative_path.display())
}
