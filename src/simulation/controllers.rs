//! Webots controller binary discovery, build, and provisioning.

use crate::webots_stage_root;
use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use anyhow::bail;
use phoxal_cli_core::project::launch_plan::SIMULATOR_CONTROLLER_ARTIFACT_NAME;
use phoxal_cli_core::project::resolver::ResolvedPlatformRuntime;
use phoxal_cli_core::project::resolver::ResolvedRobot;
use std::path::Path;
use std::path::PathBuf;

/// Stage resolved Webots controller binaries into its controller layout.
pub(crate) fn stage_simulator_controller_binaries(
    resolved: &ResolvedRobot,
    ui: &crate::Ui,
) -> Result<()> {
    let runtime = resolved_controller_runtime(&resolved.simulators)?;
    stage_controller_runtime(runtime, ui)
}

fn stage_controller_runtime(runtime: &ResolvedPlatformRuntime, ui: &crate::Ui) -> Result<()> {
    let webots_home = detected_webots_home_for_build_env();
    stage_controller_runtime_with_home(runtime, ui, webots_home.as_deref())
}

fn stage_controller_runtime_with_home(
    runtime: &ResolvedPlatformRuntime,
    ui: &crate::Ui,
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
    let resolved_binary = if let Some(crate_dir) = runtime.source_path() {
        let preferred_name = format!("phoxal-simulator-{}", runtime.name);
        let _env_guard = webots_home.map(WebotsHomeEnvGuard::set);
        crate::run::build_source_binary(crate_dir, &preferred_name, ui, None).with_context(
            || {
                format!(
                    "failed to build path-overridden simulator '{}' from {}",
                    runtime.name,
                    crate_dir.display()
                )
            },
        )?
    } else {
        provisioned_official_simulator_binary(runtime)?
    };
    let staged_dir = webots_stage_root::controller_dir(controller_name)?;
    std::fs::create_dir_all(&staged_dir).with_context(|| {
        format!(
            "failed to create staged controller directory {}",
            staged_dir.display()
        )
    })?;
    let staged_binary = staged_dir.join(controller_name);
    std::fs::copy(&resolved_binary, &staged_binary).with_context(|| {
        format!(
            "failed to copy simulator binary {} to staged controller path {}",
            resolved_binary.display(),
            staged_binary.display()
        )
    })?;
    ui.info(format!(
        "staged simulator controller binary {} at {} (copied from {})",
        runtime.name,
        staged_binary.display(),
        resolved_binary.display()
    ));
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
        Some(crate::simulate_staging::WEBOTS_CONTROLLER_NAME)
    } else {
        None
    }
}

/// Obtain the cached native-artifact binary path for a SUITE (non
/// path-overridden) simulator runtime, mirroring how
/// `crate::stager::resolve_platform_source` resolves every other official
/// artifact. Errors clearly rather than leaving the controller silently
/// unstaged when the artifact was never vendored into the project store.
pub(crate) fn provisioned_official_simulator_binary(
    runtime: &ResolvedPlatformRuntime,
) -> Result<PathBuf> {
    let descriptor = phoxal_cli_core::artifacts::NativeArtifactDescriptor::from_runtime(runtime)
        .with_context(|| {
            format!(
                "failed to resolve native-artifact descriptor for simulator '{}'",
                runtime.name
            )
        })?
        .ok_or_else(|| {
            anyhow!(
                "simulator '{}' has no built native artifact for this target; run `phoxal update` or pin a path override",
                runtime.name
            )
        })?;
    let cached = crate::native_artifacts::artifact_binary_path(&descriptor).with_context(|| {
        format!(
            "failed to locate vendored simulator '{}' in the artifact store",
            runtime.name
        )
    })?;
    if !cached.is_file() {
        bail!(
            "NativePending: simulator '{}' binary is not vendored ({}); run `phoxal update` to fetch it",
            runtime.name,
            cached.display()
        );
    }
    Ok(cached)
}

/// The Webots-linked simulator crates need `WEBOTS_HOME` to build (their
/// `phoxal-api`/webots-sys build script links against the Webots controller
/// library). `build_source_binary` inherits the CLI process environment, so
/// when the live simulate flow already has `WEBOTS_HOME` set (or the caller
/// relies on the orchestrator to set it) this is a no-op; this only fills the
/// gap defensively when host_doctor can detect an install but the process
/// environment does not already carry `WEBOTS_HOME`.
pub(crate) fn detected_webots_home_for_build_env() -> Option<PathBuf> {
    if std::env::var_os("WEBOTS_HOME").is_some() {
        return None;
    }
    crate::host_doctor::webots_home_path().ok()
}

/// RAII guard that sets `WEBOTS_HOME` for the duration of a `build_source_binary`
/// call when the process environment does not already carry it, and restores
/// the previous (absent) state afterwards. Process env mutation is otherwise
/// unsafe to interleave with other threads; live simulate's staging runs
/// single-threaded ahead of any concurrent build, so this is scoped as
/// tightly as possible and only used when `WEBOTS_HOME` was confirmed absent.
pub(crate) struct WebotsHomeEnvGuard;

impl WebotsHomeEnvGuard {
    fn set(home: &Path) -> Self {
        // SAFETY: staging runs before any concurrent participant build is
        // spawned in the live-simulate path, and this guard only ever sets a
        // variable it first confirmed was absent (`detected_webots_home_for_build_env`).
        unsafe {
            std::env::set_var("WEBOTS_HOME", home);
        }
        Self
    }
}

impl Drop for WebotsHomeEnvGuard {
    fn drop(&mut self) {
        // SAFETY: see `set` - this only ever clears the variable this guard set.
        unsafe {
            std::env::remove_var("WEBOTS_HOME");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host_paths::test_support::ScratchPhoxalHome;
    use phoxal_cli_core::project::suite::ArtifactKind;

    fn controller_runtime() -> ResolvedPlatformRuntime {
        ResolvedPlatformRuntime {
            name: SIMULATOR_CONTROLLER_ARTIFACT_NAME.to_string(),
            package: "phoxal/simulator-webots-controller".to_string(),
            kind: ArtifactKind::Simulator,
            version: "0.40.1".to_string(),
            artifact_ref: "phoxal/simulator-webots-controller@0.40.1".to_string(),
            sha256: None,
            url: None,
            size: None,
            published: true,
            published_triples: Vec::new(),
            path_override: None,
            train: "0.40.1".to_string(),
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
    fn suite_controller_missing_from_cache_is_a_hard_error() -> Result<()> {
        let _home = ScratchPhoxalHome::new()?;
        webots_stage_root::wipe_and_recreate()?;
        let error =
            stage_controller_runtime_with_home(&controller_runtime(), &crate::Ui::from_env(), None)
                .expect_err("missing suite controller must fail");
        assert!(
            format!("{error:#}").contains("failed to locate vendored simulator"),
            "{error:#}"
        );
        Ok(())
    }

    #[test]
    fn path_overridden_controller_is_built_and_staged() -> Result<()> {
        let _home = ScratchPhoxalHome::new()?;
        let source = tempfile::tempdir()?;
        std::fs::create_dir_all(source.path().join("src"))?;
        std::fs::write(
            source.path().join("Cargo.toml"),
            r#"[package]
name = "fixture-webots-controller"
version = "0.1.0"
edition = "2024"

[[bin]]
name = "phoxal-simulator-webots-controller"
path = "src/main.rs"
"#,
        )?;
        std::fs::write(source.path().join("src/main.rs"), "fn main() {}\n")?;
        std::fs::write(
            source.path().join("Cargo.lock"),
            r#"# This file is automatically @generated by Cargo.
version = 4

[[package]]
name = "fixture-webots-controller"
version = "0.1.0"
"#,
        )?;
        let mut runtime = controller_runtime();
        runtime.path_override = Some(source.path().to_path_buf());
        runtime.artifact_ref = format!("path:{}", source.path().display());
        webots_stage_root::wipe_and_recreate()?;
        stage_controller_runtime_with_home(&runtime, &crate::Ui::from_env(), None)?;
        let staged =
            webots_stage_root::controller_dir(crate::simulate_staging::WEBOTS_CONTROLLER_NAME)?
                .join(crate::simulate_staging::WEBOTS_CONTROLLER_NAME);
        assert!(
            staged.is_file(),
            "controller was not staged at {}",
            staged.display()
        );
        Ok(())
    }
}
