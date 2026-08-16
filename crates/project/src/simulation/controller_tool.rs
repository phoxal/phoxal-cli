//! Materializing the Webots controller as a CLI-owned host tool.
//!
//! The controller is not a robot runtime. It never appears in a manifest, never
//! lands in `bin/`, and never runs on the robot: it is the program Webots
//! launches on an operator's machine to simulate the robot's components. It
//! therefore rides its own release train - the repository that owns it
//! publishes on its own cadence - and the version this CLI drives is a CLI
//! constant, not the framework train.
//!
//! It is materialized into a version-keyed cache beside the CLI's other host
//! state, so a second simulation reuses the same binary rather than reinstalling
//! it. The development override (`PHOXAL_SIMULATOR_WEBOTS_PATH`) installs from a
//! checkout instead, and always with `--force`: a checkout's version does not
//! change when its source does, and Cargo would otherwise refuse to replace an
//! already-installed package of the same version with the code just edited.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, ensure};
use phoxal_cli_catalog::{
    REGISTRY_NAME, WEBOTS_CONTROLLER_PACKAGE, WEBOTS_CONTROLLER_VERSION, registry_config_arg,
};

use crate::build::materialise::MaterializeSource;
use crate::build::overlay;

/// Materialize the Webots controller and return the executable to stage.
pub(crate) fn materialize(offline: bool, reporter: &dyn crate::Reporter) -> Result<PathBuf> {
    let source = overlay::webots_controller_source();
    let root = cache_root(&source)?;
    let binary = root.join("bin").join(WEBOTS_CONTROLLER_PACKAGE);
    if matches!(source, MaterializeSource::Registry) && binary.is_file() {
        return Ok(binary);
    }
    crate::progress::run_phase(
        reporter,
        crate::progress_phase::PhaseId::new("materialize-webots-controller"),
        format!("Materializing {WEBOTS_CONTROLLER_PACKAGE}"),
        || install(&source, &root, offline, reporter),
    )?;
    ensure!(
        binary.is_file(),
        "cargo install reported success but {} is missing",
        binary.display()
    );
    Ok(binary)
}

fn install(
    source: &MaterializeSource,
    root: &Path,
    offline: bool,
    reporter: &dyn crate::Reporter,
) -> Result<()> {
    let mut command = Command::new("cargo");
    command.arg("install");
    match source {
        MaterializeSource::Registry => {
            command.args([
                format!("{WEBOTS_CONTROLLER_PACKAGE}@{WEBOTS_CONTROLLER_VERSION}"),
                "--registry".to_string(),
                REGISTRY_NAME.to_string(),
                "--config".to_string(),
                registry_config_arg(),
            ]);
        }
        MaterializeSource::Path(path) => {
            command.arg("--path").arg(path).arg("--force");
        }
    }
    command.arg("--locked").arg("--root").arg(root);
    if offline {
        command.arg("--offline");
    }
    let status = crate::build::shell::command_status_captured(&mut command, reporter)
        .with_context(|| format!("failed to run `cargo install` for {WEBOTS_CONTROLLER_PACKAGE}"))?;
    ensure!(
        status.success(),
        "cargo install {WEBOTS_CONTROLLER_PACKAGE} failed with status {status}"
    );
    Ok(())
}

/// Where one materialization of the controller lives.
///
/// A registry install is keyed by the exact version, so upgrading the constant
/// installs beside the old binary rather than over it. A development checkout
/// gets one shared directory: its version says nothing about its contents, so
/// keying by version would only accumulate stale copies of the same name.
fn cache_root(source: &MaterializeSource) -> Result<PathBuf> {
    let base = dirs::cache_dir()
        .or_else(dirs::home_dir)
        .context("failed to resolve a host cache directory for the Webots controller")?
        .join("phoxal")
        .join("webots-controller");
    Ok(match source {
        MaterializeSource::Registry => base.join(WEBOTS_CONTROLLER_VERSION),
        MaterializeSource::Path(_) => base.join("development"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A published version and a development checkout never share a cache
    /// directory: one is content-addressed by its version and the other is not
    /// addressable at all.
    #[test]
    fn a_development_checkout_never_shares_the_published_cache() -> Result<()> {
        let published = cache_root(&MaterializeSource::Registry)?;
        let development = cache_root(&MaterializeSource::Path(PathBuf::from("/checkout")))?;
        assert_ne!(published, development);
        assert!(published.ends_with(WEBOTS_CONTROLLER_VERSION));
        assert!(development.ends_with("development"));
        Ok(())
    }
}
