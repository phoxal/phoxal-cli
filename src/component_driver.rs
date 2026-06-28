use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::resolver::{ResolvedComponent, ResolvedComponentSource};
use crate::utils::resolve_project_path;
use crate::{host_paths, shell};

pub(crate) fn component_crate_dir(
    component: &ResolvedComponent,
    project_root: &Path,
) -> Result<PathBuf> {
    match &component.source {
        ResolvedComponentSource::Path { path } => Ok(resolve_project_path(project_root, path)),
        ResolvedComponentSource::Git {
            git,
            commit,
            directory,
            ..
        } => {
            let repo_dir = ensure_component_repo_cache(&component.source_name, git, commit)?;
            component_repo_subdir(repo_dir, directory.as_deref())
        }
    }
}

fn ensure_component_repo_cache(source_name: &str, git: &str, commit: &str) -> Result<PathBuf> {
    let cache_dir = host_paths::cache_dir()?
        .join("components")
        .join(format!("{source_name}-{commit}"));
    if cache_dir.join(".git").is_dir() {
        return Ok(cache_dir);
    }
    if cache_dir.exists() {
        bail!(
            "component cache {} already exists but is not a git checkout",
            cache_dir.display()
        );
    }

    let parent = cache_dir
        .parent()
        .context("component cache path did not have a parent directory")?;
    fs::create_dir_all(parent)
        .with_context(|| format!("failed to create component cache {}", parent.display()))?;
    let staging = tempfile::TempDir::new_in(parent)
        .with_context(|| format!("failed to create staging cache in {}", parent.display()))?;
    let staging_arg = staging.path().to_string_lossy().to_string();
    shell::run_status("git", ["clone", "--no-checkout", git, &staging_arg], None)
        .with_context(|| format!("failed to clone component source {git}"))?;
    shell::run_status(
        "git",
        ["checkout", "--detach", commit],
        Some(staging.path()),
    )
    .with_context(|| format!("failed to check out component source {git} at {commit}"))?;

    match fs::rename(staging.path(), &cache_dir) {
        Ok(()) => {}
        Err(error) if cache_dir.join(".git").is_dir() => {
            tracing::debug!(
                "component cache {} appeared during clone; keeping existing destination ({error})",
                cache_dir.display()
            );
        }
        Err(error) => {
            return Err(error).with_context(|| {
                format!("failed to move component cache to {}", cache_dir.display())
            });
        }
    }
    Ok(cache_dir)
}

fn component_repo_subdir(repo_root: PathBuf, directory: Option<&Path>) -> Result<PathBuf> {
    let Some(directory) = directory else {
        return Ok(repo_root);
    };
    for part in directory.components() {
        if !matches!(part, std::path::Component::Normal(_)) {
            bail!(
                "component source directory {} must be a relative path without '..'",
                directory.display()
            );
        }
    }
    Ok(repo_root.join(directory))
}
