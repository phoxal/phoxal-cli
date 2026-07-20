//! Active-version retargeting.

use super::artifact_package_dir;
use anyhow::Context;
use anyhow::Result;
use phoxal_cli_core::artifacts::NativeArtifactDescriptor;
use std::fs;
use std::path::Path;

pub(crate) fn retarget_active(descriptor: &NativeArtifactDescriptor) -> Result<()> {
    retarget_active_version(&descriptor.package_id, &descriptor.version)
}

pub(crate) fn retarget_active_version(package: &str, version: &str) -> Result<()> {
    let package_dir = artifact_package_dir(package)?;
    fs::create_dir_all(package_dir.join("versions"))?;
    let partial = package_dir.join(".active.partial");
    if fs::symlink_metadata(&partial).is_ok() {
        fs::remove_file(&partial)?;
    }
    #[cfg(unix)]
    std::os::unix::fs::symlink(Path::new("versions").join(version), &partial)?;
    #[cfg(windows)]
    std::os::windows::fs::symlink_dir(Path::new("versions").join(version), &partial)?;
    fs::rename(&partial, package_dir.join("active"))
        .context("failed to atomically retarget the active artifact version")
}
