use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};

use phoxal_cli_core::project::resolver::ResolvedComponent;

/// Locate the on-disk source directory for a component instance's resolved
/// `component_driver` package: a workspace/path-overridden crate the CLI
/// builds. Only reached for a `Path`-sourced driver - a registry-sourced
/// driver is materialized by the candidate-wide staging planner and never
/// needs a directory.
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
