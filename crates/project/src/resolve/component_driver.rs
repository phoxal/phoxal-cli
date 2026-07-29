use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};

use phoxal_cli_core::project::resolver::ResolvedComponent;

/// Locate the on-disk source directory for a component instance's resolved
/// `component_driver` package: a workspace/path-overridden crate the CLI
/// builds. Only reached for a `Path`-sourced driver - a registry-sourced
/// driver materializes straight to a `bin/` binary via `cargo install` and
/// never needs a directory (see `crate::stage::materialize_component_driver`).
/// Errors if the instance has no resolved driver package (a driverless
/// instance, or one whose `driver:` block failed to resolve - callers only
/// reach this for instances known to have a driver).
pub(crate) fn component_driver_crate_dir(
    component: &ResolvedComponent,
    _project_root: &Path,
) -> Result<PathBuf> {
    let driver = component.driver.as_ref().ok_or_else(|| {
        anyhow!(
            "component instance '{}' has no resolved component_driver package",
            component.instance
        )
    })?;
    driver
        .path_override()
        .map(Path::to_path_buf)
        .with_context(|| {
            format!(
                "component instance '{}' driver package has no on-disk directory; it materializes \
             via `cargo install`, not from a crate directory",
                component.instance
            )
        })
}

/// Locate the on-disk source directory for a component instance's resolved
/// `component_assets` package (`component.yaml`, `structure.urdf`,
/// `simulation.yaml`, `meshes/`). Resolution already settled this directory -
/// a workspace crate directory or a registry package's extraction directory
/// `cargo metadata` reported against the generated
/// `.phoxal/resolve/Cargo.toml` - so both sources collapse into the same
/// single read.
pub(crate) fn component_assets_dir(
    component: &ResolvedComponent,
    _project_root: &Path,
) -> Result<PathBuf> {
    component
        .assets
        .path_override()
        .map(Path::to_path_buf)
        .with_context(|| {
            format!(
                "component instance '{}' has no resolved component_assets directory",
                component.instance
            )
        })
}
