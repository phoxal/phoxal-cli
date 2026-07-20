//! Atomic package activation, rollback, and inactive-version pruning.

use super::{artifact_package_dir, retarget_active_version};
use anyhow::Context;
use anyhow::Result;
use phoxal_cli_core::artifacts::NativeArtifactDescriptor;
use std::fs;
use std::path::Path;

pub(crate) fn activate_packages_atomically(
    package_versions: &std::collections::BTreeMap<&String, &String>,
) -> Result<()> {
    let previous = package_versions
        .keys()
        .map(|package| {
            let active = artifact_package_dir(package)?.join("active");
            let target = fs::read_link(&active).ok();
            Ok(((*package).clone(), target))
        })
        .collect::<Result<Vec<_>>>()?;
    let mut activated: Vec<String> = Vec::new();
    for (package, version) in package_versions {
        if let Err(error) = retarget_active_version(package, version) {
            for restored_package in activated.into_iter().rev() {
                if let Some((_, target)) = previous
                    .iter()
                    .find(|(candidate, _)| candidate == &restored_package)
                {
                    restore_active_link(&restored_package, target.as_deref()).ok();
                }
            }
            return Err(error);
        }
        activated.push(package.to_string());
    }
    Ok(())
}

pub(crate) fn restore_active_link(package: &str, target: Option<&Path>) -> Result<()> {
    let package_dir = artifact_package_dir(package)?;
    let active = package_dir.join("active");
    let partial = package_dir.join(".active.rollback");
    if fs::symlink_metadata(&partial).is_ok() {
        fs::remove_file(&partial)?;
    }
    let Some(target) = target else {
        if fs::symlink_metadata(&active).is_ok() {
            fs::remove_file(active)?;
        }
        return Ok(());
    };
    #[cfg(unix)]
    std::os::unix::fs::symlink(target, &partial)?;
    #[cfg(windows)]
    std::os::windows::fs::symlink_dir(target, &partial)?;
    fs::rename(partial, active)?;
    Ok(())
}

pub(crate) fn prune_inactive_versions(
    package_versions: &std::collections::BTreeMap<&String, &String>,
) -> Result<()> {
    for (package, selected) in package_versions {
        let versions = artifact_package_dir(package)?.join("versions");
        if !versions.is_dir() {
            continue;
        }
        for entry in fs::read_dir(&versions)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() || entry.file_name() == selected.as_str() {
                continue;
            }
            fs::remove_dir_all(entry.path()).with_context(|| {
                format!(
                    "failed to prune inactive artifact version {}",
                    entry.path().display()
                )
            })?;
        }
    }
    Ok(())
}

pub(crate) fn inactive_versions(descriptor: &NativeArtifactDescriptor) -> Result<Vec<String>> {
    let versions = artifact_package_dir(&descriptor.package_id)?.join("versions");
    if !versions.is_dir() {
        return Ok(Vec::new());
    }
    let mut inactive = Vec::new();
    for entry in fs::read_dir(versions)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            let version = entry.file_name().to_string_lossy().into_owned();
            if version != descriptor.version && !version.starts_with('.') {
                inactive.push(version);
            }
        }
    }
    inactive.sort();
    Ok(inactive)
}
