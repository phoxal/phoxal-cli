use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};

use crate::git_artifact;
use crate::native_artifacts::{self, NativeArtifactDescriptor, ProvisioningMode};
use crate::resolver::{ResolvedComponent, ResolvedComponentPackage, ResolvedComponentSource};
use crate::utils::resolve_project_path;

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
    resolved_component_package_dir(driver, project_root)
}

/// Locate the on-disk source directory for a component instance's resolved
/// `component_assets` package (`component.yaml`, `structure.urdf`,
/// `simulation.yaml`, `meshes/`). Always present - see [`ResolvedComponent`].
pub(crate) fn component_assets_dir(
    component: &ResolvedComponent,
    project_root: &Path,
) -> Result<PathBuf> {
    resolved_component_package_dir(&component.assets, project_root)
}

/// A component's `Git` source now routes through the general
/// `crate::git_artifact` resolver (any `robot.yaml` `artifacts.pins` entry
/// can be git-sourced, not just components; see `resolver::apply_path_pins`)
/// instead of the old component-only `cache/components/` cache.
fn resolved_component_package_dir(
    package: &ResolvedComponentPackage,
    project_root: &Path,
) -> Result<PathBuf> {
    match &package.source {
        ResolvedComponentSource::Path { path } => Ok(resolve_project_path(project_root, path)),
        ResolvedComponentSource::Git {
            git,
            rev,
            directory,
        } => {
            let repo_dir = git_artifact::ensure_git_artifact(git, rev)?;
            git_artifact::subdir(repo_dir, directory.as_deref())
        }
        ResolvedComponentSource::Catalog => catalog_component_package_dir(package),
    }
}

/// Fetch and unpack a catalog-resolved component package's release asset via
/// the identical native-staging path services/tools already use
/// ([`native_artifacts::stage_component_package`]/`stage_descriptor`), and
/// return its local exec directory: the unpacked assets bundle root
/// (`component.yaml`, `structure.urdf`, `simulation.yaml`, `meshes/`) for a
/// `component_assets` package, or the directory containing the staged driver
/// binary for a `component_driver` package.
/// `MissingOnly` mode: reuses an already-staged local unpack without touching
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
    native_artifacts::artifact_exec_dir(&descriptor)
}
