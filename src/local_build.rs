use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};

use crate::AppContext;
use crate::resolver::{ResolvedComponentSource, ResolvedRobot};
use crate::{host_paths, shell};

pub(crate) fn pull_platform_images(app: &AppContext, resolved: &ResolvedRobot) -> Result<()> {
    for runtime in &resolved.platform_runtimes {
        // `deploy_ref` is a real digest pin or an honest tag ref — never a
        // fabricated `sha256:` — so this can't attempt to pull a fake digest.
        let image = runtime.deploy_ref();
        app.ui.info(format!("pulling {image}"));
        shell::run_status("docker", ["pull", image.as_str()], None).with_context(|| {
            if runtime.digest_pin().is_some() {
                format!("failed to pull pinned runtime image {image}")
            } else {
                format!(
                    "failed to pull runtime image {image} by tag. The phoxal/framework GHCR \
                     runtime images may not be published for this runtime set yet. Publish the \
                     runtime images, then run `phoxal-cli update --pin-digests` to pin real \
                     digests, or `phoxal-cli update --refresh-releases` to pick up a newer set."
                )
            }
        })?;
    }
    Ok(())
}

pub(crate) fn build_user_runtimes(
    project_root: &Path,
    resolved: &ResolvedRobot,
) -> Result<BTreeMap<String, String>> {
    let mut images = BTreeMap::new();
    for runtime in &resolved.user_runtimes {
        let runtime_dir = resolve_project_path(project_root, &runtime.path);
        let hash = hash_tree(&runtime_dir)?;
        let image = format!(
            "phoxal-local/{}/user-runtime/{}:{}",
            resolved.robot.identity.id, runtime.name, hash
        );
        shell::run_status(
            "docker",
            ["build", "-t", image.as_str(), "."],
            Some(&runtime_dir),
        )?;
        images.insert(runtime.name.clone(), image);
    }
    Ok(images)
}

pub(crate) fn build_component_drivers(project_root: &Path, resolved: &ResolvedRobot) -> Result<()> {
    let host_cache_dir = host_paths::cache_dir()?;
    for component in &resolved.components {
        if !component.has_driver {
            continue;
        }
        let driver_dir = match &component.source {
            ResolvedComponentSource::Path { path } => {
                resolve_project_path(project_root, path).join("driver")
            }
            ResolvedComponentSource::Git { commit, .. } => host_cache_dir
                .join("components")
                .join(format!("{}-{commit}", component.source_name))
                .join("driver"),
        };
        if driver_dir.is_dir() {
            shell::run_status("cargo", ["build", "--release"], Some(&driver_dir))?;
        }
    }
    Ok(())
}

pub(crate) fn collect_files_under(root: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    collect_absolute_files(root, &mut files)?;
    files.sort();
    Ok(files)
}

fn collect_absolute_files(path: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
    for entry in fs::read_dir(path).with_context(|| format!("failed to read {}", path.display()))? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            collect_absolute_files(&path, files)?;
        } else if path.is_file() {
            files.push(path);
        }
    }
    Ok(())
}

fn hash_tree(path: &Path) -> Result<String> {
    let mut files = Vec::new();
    collect_files(path, path, &mut files)?;
    files.sort();
    let mut hasher = Sha256::new();
    for file in files {
        hasher.update(file.to_string_lossy().as_bytes());
        hasher.update(fs::read(path.join(&file))?);
    }
    Ok(hex::encode(hasher.finalize())[..16].to_string())
}

fn collect_files(root: &Path, path: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
    for entry in fs::read_dir(path).with_context(|| format!("failed to read {}", path.display()))? {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name();
        if name == ".git" || name == "target" {
            continue;
        }
        if path.is_dir() {
            collect_files(root, &path, files)?;
        } else if path.is_file() {
            files.push(path.strip_prefix(root)?.to_path_buf());
        }
    }
    Ok(())
}

fn resolve_project_path(project_root: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        project_root.join(path)
    }
}
