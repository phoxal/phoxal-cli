//! Webots controller binary discovery, build, and provisioning.
//!
//! The controller materializes into its own root, `.phoxal/simulation/`
//! (organization#951 WS4) - separate from `.phoxal/bundle/`, the deployed
//! robot bundle - and is built only when a simulation is requested. It is
//! exposed to Webots by a SYMLINK at
//! `.phoxal/webots/controllers/<name>/<name>`, never a copy: staging replaces
//! binaries by atomic rename (a new inode every time), so a hardlink or copy
//! would keep resolving to the previous build and silently run a stale
//! controller, while a symlink resolves through the path on every exec.

use crate::simulation::webots::root;
use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use phoxal_cli_core::project::launch_plan::{
    SIMULATOR_CONTROLLER_ARTIFACT_NAME, simulation_root_dir,
};
use phoxal_cli_core::project::resolver::ResolvedPlatformRuntime;
use phoxal_cli_core::project::resolver::ResolvedRobot;
use std::ffi::OsString;
use std::path::Path;

/// Stage the resolved Webots controller binary into its controller layout:
/// materialize it into `.phoxal/simulation/bin/`, then symlink it at
/// `.phoxal/webots/controllers/<name>/<name>`.
pub(crate) fn stage_simulator_controller_binaries(
    project_root: &Path,
    resolved: &ResolvedRobot,
    offline: bool,
    ui: &dyn crate::Reporter,
    webots_home: Option<&Path>,
) -> Result<()> {
    let runtime = resolved_controller_runtime(&resolved.simulators)?;
    stage_controller_runtime_with_home(project_root, runtime, offline, ui, webots_home)
}

fn stage_controller_runtime_with_home(
    project_root: &Path,
    runtime: &ResolvedPlatformRuntime,
    offline: bool,
    ui: &dyn crate::Reporter,
    webots_home: Option<&Path>,
) -> Result<()> {
    let controller_name =
        webots_controller_name_for_simulator_artifact(&runtime.name).ok_or_else(|| {
            anyhow!(
                "unrecognized simulator artifact '{}'; expected '{}'",
                runtime.name,
                SIMULATOR_CONTROLLER_ARTIFACT_NAME
            )
        })?;
    let simulation_root = simulation_root_dir(project_root);
    let resolved_binary = if let Some(crate_dir) = runtime.source_path() {
        let preferred_name = format!("phoxal-simulator-{}", runtime.name);
        let _env_guard = webots_home.map(WebotsHomeEnvGuard::set);
        // Simulation is an interactive development path, so source and
        // registry controller variants both use Cargo's debug profile.
        let built = crate::build::cargo::build_source_binary_with_profile(
            crate_dir,
            &preferred_name,
            ui,
            None,
            crate::build::profile::Profile::Debug,
            offline,
        )
        .with_context(|| {
            format!(
                "failed to build path-overridden simulator '{}' from {}",
                runtime.name,
                crate_dir.display()
            )
        })?;
        crate::stage::stage_named_binary(&simulation_root, &preferred_name, &built)?
    } else {
        let target_dir = crate::build::cargo::cargo_target_dir(project_root, offline)?;
        let spec = crate::build::materialise::MaterializeSpec::new(
            runtime.package.clone(),
            runtime.train.clone(),
        )
        .with_target(runtime.target.clone())
        .with_profile(crate::build::materialise::MaterializeProfile::Debug)
        .with_target_dir(target_dir);
        crate::build::materialise::cargo_install(&simulation_root, &spec, offline, ui)
            .with_context(|| {
                format!(
                    "failed to materialize official simulator '{}'",
                    runtime.name
                )
            })?
    };
    let staged_dir = root::controller_dir(controller_name)?;
    std::fs::create_dir_all(&staged_dir).with_context(|| {
        format!(
            "failed to create staged controller directory {}",
            staged_dir.display()
        )
    })?;
    let staged_binary = staged_dir.join(controller_name);
    symlink_controller(&resolved_binary, &staged_binary)?;
    ui.info(format!(
        "staged simulator controller binary {} at {} (symlinked to {})",
        runtime.name,
        staged_binary.display(),
        resolved_binary.display()
    ));
    Ok(())
}

/// Symlink `resolved_binary` at `staged_binary`, replacing any previous
/// entry. A symlink - never a hardlink or copy - so a controller rebuild
/// (which lands at a new inode via atomic rename) is picked up on the very
/// next Webots exec instead of silently running the previous build.
fn symlink_controller(resolved_binary: &Path, staged_binary: &Path) -> Result<()> {
    if staged_binary.symlink_metadata().is_ok() {
        std::fs::remove_file(staged_binary).with_context(|| {
            format!(
                "failed to remove stale controller symlink {}",
                staged_binary.display()
            )
        })?;
    }
    #[cfg(unix)]
    std::os::unix::fs::symlink(resolved_binary, staged_binary).with_context(|| {
        format!(
            "failed to symlink controller {} -> {}",
            staged_binary.display(),
            resolved_binary.display()
        )
    })?;
    #[cfg(windows)]
    std::os::windows::fs::symlink_file(resolved_binary, staged_binary).with_context(|| {
        format!(
            "failed to symlink controller {} -> {}",
            staged_binary.display(),
            resolved_binary.display()
        )
    })?;
    Ok(())
}

fn resolved_controller_runtime(
    simulators: &[ResolvedPlatformRuntime],
) -> Result<&ResolvedPlatformRuntime> {
    let controllers = simulators
        .iter()
        .filter(|runtime| runtime.name == SIMULATOR_CONTROLLER_ARTIFACT_NAME)
        .collect::<Vec<_>>();
    anyhow::ensure!(
        controllers.len() == 1,
        "Webots simulation requires exactly one '{}' runtime, resolved {}",
        SIMULATOR_CONTROLLER_ARTIFACT_NAME,
        controllers.len()
    );
    Ok(controllers[0])
}

/// Map a resolved simulator artifact name to its Webots controller directory
/// name (the value that must appear in the staged world's `controller "..."`
/// field and the `controllers/<name>/<name>` staged path) - the inverse
/// mapping of participant ids, but keyed to the on-disk Webots layout instead
/// of the bus participant id.
pub(crate) fn webots_controller_name_for_simulator_artifact(
    artifact_name: &str,
) -> Option<&'static str> {
    if artifact_name == SIMULATOR_CONTROLLER_ARTIFACT_NAME {
        Some(crate::simulation::prepare::WEBOTS_CONTROLLER_NAME)
    } else {
        None
    }
}

/// RAII guard that sets `WEBOTS_HOME` for the duration of a `build_source_binary`
/// call and restores the exact previous value afterwards.
pub(crate) struct WebotsHomeEnvGuard {
    previous: Option<OsString>,
}

impl WebotsHomeEnvGuard {
    fn set(home: &Path) -> Self {
        let previous = std::env::var_os("WEBOTS_HOME");
        // SAFETY: this mutation is scoped to the synchronous controller build;
        // the previous process value is retained and restored by Drop.
        unsafe {
            std::env::set_var("WEBOTS_HOME", home);
        }
        Self { previous }
    }
}

impl Drop for WebotsHomeEnvGuard {
    fn drop(&mut self) {
        // SAFETY: see `set`; restore precisely what was present before.
        unsafe {
            if let Some(previous) = &self.previous {
                std::env::set_var("WEBOTS_HOME", previous);
            } else {
                std::env::remove_var("WEBOTS_HOME");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::host::test_support::ScratchPhoxalHome;
    use phoxal_cli_core::project::catalog::ArtifactKind;
    use std::sync::Mutex;

    static WEBOTS_HOME_TEST_LOCK: Mutex<()> = Mutex::new(());

    fn controller_runtime() -> ResolvedPlatformRuntime {
        ResolvedPlatformRuntime {
            name: SIMULATOR_CONTROLLER_ARTIFACT_NAME.to_string(),
            package: "phoxal/simulator-webots-controller".to_string(),
            kind: ArtifactKind::Simulator,
            path_override: None,
            train: "0.40.2".to_string(),
            target: None,
        }
    }

    #[test]
    fn exactly_one_controller_runtime_is_required() {
        assert!(resolved_controller_runtime(&[]).is_err());
        let one = [controller_runtime()];
        assert_eq!(
            resolved_controller_runtime(&one)
                .expect("one controller")
                .name,
            SIMULATOR_CONTROLLER_ARTIFACT_NAME
        );
        let duplicate = [controller_runtime(), controller_runtime()];
        assert!(resolved_controller_runtime(&duplicate).is_err());
    }

    #[test]
    fn webots_home_guard_restores_the_exact_existing_value() {
        let _lock = WEBOTS_HOME_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let previous = std::env::var_os("WEBOTS_HOME");
        // SAFETY: this test serializes its WEBOTS_HOME mutation.
        unsafe {
            std::env::set_var("WEBOTS_HOME", "/original/webots");
        }
        {
            let _guard = WebotsHomeEnvGuard::set(Path::new("/temporary/webots"));
            assert_eq!(
                std::env::var_os("WEBOTS_HOME").as_deref(),
                Some(std::ffi::OsStr::new("/temporary/webots"))
            );
        }
        assert_eq!(
            std::env::var_os("WEBOTS_HOME").as_deref(),
            Some(std::ffi::OsStr::new("/original/webots"))
        );
        // SAFETY: restore the value present before this serialized test.
        unsafe {
            match previous {
                Some(value) => std::env::set_var("WEBOTS_HOME", value),
                None => std::env::remove_var("WEBOTS_HOME"),
            }
        }
    }

    #[test]
    fn path_overridden_controller_is_built_and_symlinked_into_the_webots_layout() -> Result<()> {
        let _home = ScratchPhoxalHome::new()?;
        let project = tempfile::tempdir()?;
        let source = project.path().join("simulator-webots-controller");
        std::fs::create_dir_all(source.join("src"))?;
        std::fs::write(
            source.join("Cargo.toml"),
            r#"[package]
name = "fixture-webots-controller"
version = "0.1.0"
edition = "2024"

[[bin]]
name = "phoxal-simulator-webots-controller"
path = "src/main.rs"
"#,
        )?;
        std::fs::write(source.join("src/main.rs"), "fn main() {}\n")?;
        std::fs::write(
            source.join("Cargo.lock"),
            r#"# This file is automatically @generated by Cargo.
version = 4

[[package]]
name = "fixture-webots-controller"
version = "0.1.0"
"#,
        )?;
        let mut runtime = controller_runtime();
        runtime.path_override = Some(source.clone());
        root::wipe_and_recreate()?;
        stage_controller_runtime_with_home(
            project.path(),
            &runtime,
            false,
            &crate::SilentReporter,
            None,
        )?;
        let staged = root::controller_dir(crate::simulation::prepare::WEBOTS_CONTROLLER_NAME)?
            .join(crate::simulation::prepare::WEBOTS_CONTROLLER_NAME);
        assert!(
            staged
                .symlink_metadata()
                .is_ok_and(|meta| meta.file_type().is_symlink()),
            "controller must be exposed as a symlink at {}",
            staged.display()
        );
        assert!(staged.is_file(), "the symlink must resolve to a real file");
        // The symlink resolves into the SEPARATE simulation root, never the
        // deployed bundle root.
        assert!(std::fs::read_link(&staged)?.starts_with(
            phoxal_cli_core::project::launch_plan::simulation_root_dir(project.path())
        ));
        Ok(())
    }
}
