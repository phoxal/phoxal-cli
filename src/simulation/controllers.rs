//! Webots controller binary discovery, build, and provisioning.

use crate::webots_stage_root;
use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use anyhow::bail;
use phoxal_cli_core::project::launch_plan::SIMULATOR_CONTROLLER_ARTIFACT_NAME;
use phoxal_cli_core::project::launch_plan::SIMULATOR_SUPERVISOR_ARTIFACT_NAME;
use phoxal_cli_core::project::resolver::ResolvedPlatformRuntime;
use phoxal_cli_core::project::resolver::ResolvedRobot;
use std::path::Path;
use std::path::PathBuf;

/// Stage resolved Webots controller binaries into its controller layout.
pub(crate) fn stage_simulator_controller_binaries(
    resolved: &ResolvedRobot,
    ui: &crate::Ui,
) -> Result<()> {
    let webots_home = detected_webots_home_for_build_env();
    for runtime in &resolved.simulators {
        let controller_name = webots_controller_name_for_simulator_artifact(&runtime.name)
            .ok_or_else(|| {
                anyhow!(
                    "unrecognized simulator artifact '{}'; expected '{}' or '{}'",
                    runtime.name,
                    SIMULATOR_SUPERVISOR_ARTIFACT_NAME,
                    SIMULATOR_CONTROLLER_ARTIFACT_NAME
                )
            })?;
        let resolved_binary = if let Some(crate_dir) = runtime.source_path() {
            let preferred_name = format!("phoxal-simulator-{}", runtime.name);
            let _env_guard = webots_home
                .as_ref()
                .map(|home| WebotsHomeEnvGuard::set(home));
            crate::run::build_source_binary(crate_dir, &preferred_name, ui).with_context(|| {
                format!(
                    "failed to build path-overridden simulator '{}' from {}",
                    runtime.name,
                    crate_dir.display()
                )
            })?
        } else {
            provisioned_official_simulator_binary(runtime)?
        };
        require_absolute_symlink_target("resolved simulator binary", &resolved_binary)?;
        let staged_dir = webots_stage_root::controller_dir(controller_name)?;
        std::fs::create_dir_all(&staged_dir).with_context(|| {
            format!(
                "failed to create staged controller directory {}",
                staged_dir.display()
            )
        })?;
        let staged_binary = staged_dir.join(controller_name);
        std::os::unix::fs::symlink(&resolved_binary, &staged_binary).with_context(|| {
            format!(
                "failed to symlink simulator binary {} to staged controller path {}",
                resolved_binary.display(),
                staged_binary.display()
            )
        })?;
        ui.info(format!(
            "staged simulator controller binary {} at {} (symlink to {})",
            runtime.name,
            staged_binary.display(),
            resolved_binary.display()
        ));
    }
    Ok(())
}

/// Symlink targets into the staged simulation must be absolute (Webots' cwd
/// when it execs `controllers/<name>/<name>` is not the staged tree, so a
/// relative symlink would not resolve). Both sources this crate ever
/// symlinks from - the native-artifact cache and a path-pinned crate's cargo
/// `target_directory` - are already absolute by construction; this asserts
/// that rather than silently trying to fix up a relative one.
pub(crate) fn require_absolute_symlink_target(label: &str, path: &Path) -> Result<()> {
    if path.is_absolute() {
        Ok(())
    } else {
        bail!(
            "{label} must be an absolute path to symlink into the staged simulation, got {}",
            path.display()
        );
    }
}

/// Map a resolved simulator artifact name to its Webots controller directory
/// name (the value that must appear in the staged world's `controller "..."`
/// field and the `controllers/<name>/<name>` staged path) - the inverse
/// mapping of participant ids, but keyed to the on-disk Webots layout instead
/// of the bus participant id.
pub(crate) fn webots_controller_name_for_simulator_artifact(
    artifact_name: &str,
) -> Option<&'static str> {
    if artifact_name == SIMULATOR_SUPERVISOR_ARTIFACT_NAME {
        Some("phoxal-simulator-webots-supervisor")
    } else if artifact_name == SIMULATOR_CONTROLLER_ARTIFACT_NAME {
        Some("phoxal-simulator-webots-controller")
    } else {
        None
    }
}

/// Obtain the cached native-artifact binary path for a SUITE (non
/// path-overridden) simulator runtime, mirroring how
/// `crate::run::locate_official_binary` resolves every other official
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
