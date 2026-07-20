//! Artifact cache paths, active-version lookup, and currentness checks.

use super::{ArtifactStoreLock, SCOPE_DIGEST_FILE, artifact_package_dir, validate_path_segment};
use anyhow::Context;
use anyhow::Result;
use phoxal_cli_core::artifacts::NativeArtifactDescriptor;
use std::fs;
use std::path::PathBuf;

pub(crate) fn descriptor_is_current(descriptor: &NativeArtifactDescriptor) -> bool {
    let Ok(path) = artifact_exec_dir(descriptor) else {
        return false;
    };
    // Normal commands deliberately resolve through the project-vendored
    // `active` link without consulting a channel head, so that offline view
    // has no catalog digest to compare. `update` resolves the live catalog
    // with a non-empty digest and therefore takes the strict branch below.
    if descriptor.sha256.is_empty() {
        return path.is_dir();
    }
    fs::read_to_string(path.join(SCOPE_DIGEST_FILE))
        .is_ok_and(|digest| digest.trim() == descriptor.sha256)
}

/// Return the selected binary through its package-scoped `active` symlink.
pub fn artifact_binary_path(descriptor: &NativeArtifactDescriptor) -> Result<PathBuf> {
    let _lock = ArtifactStoreLock::shared()?;
    let target = descriptor
        .target
        .as_deref()
        .context("component assets do not contain a native binary")?;
    validate_path_segment("artifact target", target)?;
    let version = active_version_unlocked(&descriptor.package_id)?
        .context("vendored artifact package has no active version")?;
    Ok(artifact_package_dir(&descriptor.package_id)?
        .join("versions")
        .join(version)
        .join("targets")
        .join(target)
        .join(&descriptor.binary_name))
}

/// Temporary download path beside the selected version directory.
pub fn artifact_tarball_path(descriptor: &NativeArtifactDescriptor) -> Result<PathBuf> {
    let scope = descriptor.target.as_deref().unwrap_or("assets");
    validate_path_segment("artifact scope", scope)?;
    Ok(artifact_package_dir(&descriptor.package_id)?
        .join("versions")
        .join(format!(".{}-{scope}.partial", descriptor.version)))
}

/// Where `descriptor` is unpacked in the project-local artifact store.
pub fn artifact_exec_dir(descriptor: &NativeArtifactDescriptor) -> Result<PathBuf> {
    validate_path_segment("artifact version", &descriptor.version)?;
    let version = artifact_package_dir(&descriptor.package_id)?
        .join("versions")
        .join(&descriptor.version);
    match descriptor.target.as_deref() {
        Some(target) => {
            validate_path_segment("artifact target", target)?;
            Ok(version.join("targets").join(target))
        }
        None => Ok(version.join("assets")),
    }
}

pub fn artifact_target_dir_for(package: &str, target: &str) -> Result<PathBuf> {
    validate_path_segment("artifact target", target)?;
    Ok(artifact_package_dir(package)?
        .join("active")
        .join("targets")
        .join(target))
}

pub fn artifact_assets_dir_for(package: &str) -> Result<PathBuf> {
    Ok(artifact_package_dir(package)?.join("active").join("assets"))
}

#[cfg(test)]
pub fn active_version(descriptor: &NativeArtifactDescriptor) -> Result<Option<String>> {
    active_version_for(&descriptor.package_id)
}

pub(crate) fn active_version_read_only(
    descriptor: &NativeArtifactDescriptor,
) -> Result<Option<String>> {
    active_version_unlocked(&descriptor.package_id)
}

pub fn active_version_for(package: &str) -> Result<Option<String>> {
    let _lock = ArtifactStoreLock::shared()?;
    active_version_unlocked(package)
}

pub(crate) fn active_version_unlocked(package: &str) -> Result<Option<String>> {
    let active = artifact_package_dir(package)?.join("active");
    match fs::read_link(&active) {
        Ok(target) => Ok(target
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("failed to read {}", active.display())),
    }
}

pub fn existing_target_scopes(package: &str) -> Result<Vec<String>> {
    let targets_dir = artifact_package_dir(package)?
        .join("active")
        .join("targets");
    if !targets_dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut targets = Vec::new();
    for entry in fs::read_dir(targets_dir)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            let target = entry.file_name().to_string_lossy().into_owned();
            validate_path_segment("stored artifact target", &target)?;
            targets.push(target);
        }
    }
    targets.sort();
    Ok(targets)
}
