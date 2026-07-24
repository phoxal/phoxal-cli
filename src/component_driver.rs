use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};

use crate::native_artifacts;
use phoxal_cli_core::artifacts::{NativeArtifactDescriptor, ProvisioningMode};
use phoxal_cli_core::project::resolver::{
    ResolvedComponent, ResolvedComponentPackage, ResolvedComponentSource,
};
use phoxal_cli_core::project::tooling::resolve_project_path;

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
/// `simulation.yaml`, `meshes/`). `None` when the component is driverless
/// (passive) and has no official assets package - see [`ResolvedComponent`].
pub(crate) fn component_assets_dir(
    component: &ResolvedComponent,
    project_root: &Path,
) -> Result<Option<PathBuf>> {
    component
        .assets
        .as_ref()
        .map(|assets| resolved_component_package_dir(assets, project_root))
        .transpose()
}

fn resolved_component_package_dir(
    package: &ResolvedComponentPackage,
    project_root: &Path,
) -> Result<PathBuf> {
    match &package.source {
        ResolvedComponentSource::Path { path } => Ok(resolve_project_path(project_root, path)),
        ResolvedComponentSource::Suite => suite_component_package_dir(package),
    }
}

/// Fetch and unpack a suite-resolved component package's release asset via
/// the identical native-staging path services/tools already use
/// (`native_artifacts::stage_runtime`/`stage_descriptor`), and return its
/// local exec directory: the unpacked assets bundle root
/// (`component.yaml`, `structure.urdf`, `simulation.yaml`, `meshes/`) for a
/// `component_assets` package, or the directory containing the staged driver
/// binary for a `component_driver` package.
/// `MissingOnly` mode: reuses an already-staged local unpack without touching
/// the network again, matching how a service's cache is consulted.
fn suite_component_package_dir(package: &ResolvedComponentPackage) -> Result<PathBuf> {
    let Some(runtime) = &package.suite_runtime else {
        bail!(
            "component package {} resolves from the artifact suite but has no release asset for \
             this target yet; it cannot be staged locally. Wait for it to publish or add a \
             components/ workspace crate.",
            package.package
        );
    };
    let descriptor = NativeArtifactDescriptor::from_runtime(runtime)?.ok_or_else(|| {
        anyhow!(
            "component package {} has no release asset for this target yet; it cannot be staged locally. \
             Wait for it to publish or add a components/ workspace crate.",
            package.package
        )
    })?;
    native_artifacts::stage_descriptor(None, &descriptor, ProvisioningMode::MissingOnly)
        .with_context(|| format!("failed to stage component package {}", package.package))?;
    native_artifacts::artifact_exec_dir(&descriptor)
}
