use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};

use crate::native_artifacts::{self, NativeArtifactDescriptor, ProvisioningMode};
use crate::resolver::{ResolvedComponent, ResolvedComponentPackage, ResolvedComponentSource};
use crate::utils::resolve_project_path;
use crate::{host_paths, shell};

/// Locate the on-disk source directory for a component instance's resolved
/// `component_driver` package (the crate `check`/`run`/`watch` build). Errors
/// if the instance has no resolved driver package (a driverless instance, or
/// one whose `driver:` block failed to resolve - callers only reach this for
/// instances known to have a driver).
pub(crate) fn component_driver_crate_dir(
    component: &ResolvedComponent,
    project_root: &Path,
) -> Result<PathBuf> {
    let driver = component.driver.as_ref().ok_or_else(|| {
        anyhow!(
            "component instance '{}' has no resolved component_driver package",
            component.instance
        )
    })?;
    resolved_component_package_dir(driver, &component.source_name, project_root)
}

/// Locate the on-disk source directory for a component instance's resolved
/// `component_assets` package (`component.yaml`, `structure.urdf`,
/// `simulation.yaml`, `meshes/`). Always present - see [`ResolvedComponent`].
pub(crate) fn component_assets_dir(
    component: &ResolvedComponent,
    project_root: &Path,
) -> Result<PathBuf> {
    resolved_component_package_dir(&component.assets, &component.source_name, project_root)
}

fn resolved_component_package_dir(
    package: &ResolvedComponentPackage,
    source_name: &str,
    project_root: &Path,
) -> Result<PathBuf> {
    match &package.source {
        ResolvedComponentSource::Path { path } => Ok(resolve_project_path(project_root, path)),
        ResolvedComponentSource::Git {
            git,
            rev,
            directory,
        } => {
            let repo_dir = ensure_component_repo_cache(source_name, git, rev)?;
            component_repo_subdir(repo_dir, directory.as_deref())
        }
        ResolvedComponentSource::Catalog => catalog_component_package_dir(package),
    }
}

/// Fetch and unpack a catalog-resolved component package's release asset via
/// the identical native-staging path services/tools already use
/// ([`native_artifacts::stage_component_package`]/`stage_descriptor`), and
/// return its local cache directory: the unpacked assets bundle root
/// (`component.yaml`, `structure.urdf`, `simulation.yaml`, `meshes/`) for a
/// `component_assets` package, or the cache directory containing the staged
/// driver binary for a `component_driver` package.
/// `MissingOnly` mode: reuses an already-staged local cache without touching
/// the network again, matching how a service's cache is consulted.
fn catalog_component_package_dir(package: &ResolvedComponentPackage) -> Result<PathBuf> {
    let Some(runtime) = &package.catalog_runtime else {
        bail!(
            "component package {} resolves from the artifact catalog but has no release asset for \
             this target yet; it cannot be staged locally. Wait for it to publish, or pin \
             artifacts.pins.{} to a path/git override.",
            package.package,
            package.package
        );
    };
    let descriptor = NativeArtifactDescriptor::from_runtime(runtime)?.ok_or_else(|| {
        anyhow!(
            "component package {} has no release asset for this target yet; it cannot be staged locally. \
             Wait for it to publish, or pin artifacts.pins.{} to a path/git override.",
            package.package,
            package.package
        )
    })?;
    native_artifacts::stage_descriptor(None, &descriptor, ProvisioningMode::MissingOnly)
        .with_context(|| format!("failed to stage component package {}", package.package))?;
    native_artifacts::artifact_cache_dir(&descriptor)
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
