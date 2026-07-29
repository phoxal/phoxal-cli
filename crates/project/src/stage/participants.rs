//! Participant materialization and flat binary-store staging.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use phoxal_cli_core::project::launch_plan::ParticipantLaunchRecord;
use phoxal_cli_core::project::resolver::{
    ResolvedPlatformRuntime, ResolvedRobot, ResolvedTool, official_binary_name, tool_participant_id,
};

use super::publish::remove_if_present;
use crate::build::materialise::{MaterializeProfile, MaterializeSpec, cargo_install};

const BIN_DIR: &str = "bin";

#[derive(Debug, Clone)]
pub(crate) struct MaterializeSettings {
    pub(crate) profile: MaterializeProfile,
    pub(crate) target_dir: Option<PathBuf>,
}

impl Default for MaterializeSettings {
    fn default() -> Self {
        Self {
            profile: MaterializeProfile::Release,
            target_dir: None,
        }
    }
}

impl MaterializeSettings {
    #[must_use]
    pub(crate) fn development(target_dir: PathBuf) -> Self {
        Self {
            profile: MaterializeProfile::Debug,
            target_dir: Some(target_dir),
        }
    }

    #[must_use]
    pub(crate) fn release(target_dir: PathBuf) -> Self {
        Self {
            profile: MaterializeProfile::Release,
            target_dir: Some(target_dir),
        }
    }

    pub(crate) fn apply(&self, mut spec: MaterializeSpec) -> MaterializeSpec {
        spec = spec.with_profile(self.profile);
        if let Some(target_dir) = &self.target_dir {
            spec = spec.with_target_dir(target_dir.clone());
        }
        spec
    }
}

/// The canonical identity name one launched participant is stored under in
/// `bin/`. The source-free plan (#936) names each participant's `bin/` binary
/// on its `execution` directly - the loader resolves the identical name from
/// the compiled `robot.yaml` - so this is just that name.
pub(super) fn canonical_binary_name(participant: &ParticipantLaunchRecord) -> String {
    participant.execution.binary_name().to_string()
}

/// Stage one launched CLI-managed participant's binary into the staged `bin/`
/// under its canonical identity name, returning the flat `bin/` entry the
/// participant runs from. `source` is the built binary a workspace/source
/// override resolves to; `bin/` is a flat identity-keyed store, so one driver
/// binary shared by several component instances is linked once under its
/// component id, and a source-overridden official lands at the same name a
/// `cargo install`-materialized one would - the loader resolves both
/// identically.
///
/// No symlinks and no `.app` bundles: the staged layout holds real file
/// identities that keep working if `target/` is later cleaned. Every
/// participant, on every host, gets a flat `bin/` entry - a robot participant is
/// always a plain executable (a cargo-built `target/` binary), never a macOS
/// `.app` bundle. The only `.app` in the whole system is the Webots
/// *application* on the `simulate` path, which is a CLI-managed host process
/// outside the plan/layout contract entirely (its bundle is handled by the
/// supervisor's `materialize_macos_app_binary`, never here).
pub(crate) fn stage_participant_binary(
    staged_root: &Path,
    participant: &ParticipantLaunchRecord,
    source: &Path,
) -> Result<PathBuf> {
    stage_named_binary(staged_root, &canonical_binary_name(participant), source)
}

/// Stage one resolved source binary into the staged `bin/` under an explicit
/// canonical name, returning the flat `bin/` entry the participant runs from.
/// This is the name-keyed core [`stage_participant_binary`] delegates to, and
/// the entry point the layout-completing staging pass (#936) uses to link user
/// services and component drivers into `bin/` before the loader constructs the
/// plan. Strict flat `bin/` on every host - see [`stage_participant_binary`].
pub(crate) fn stage_named_binary(
    staged_root: &Path,
    binary_name: &str,
    source: &Path,
) -> Result<PathBuf> {
    let bin_dir = ensure_bin_dir(staged_root)?;
    let staged = bin_dir.join(binary_name);
    link_or_copy(source, &staged)?;
    Ok(staged)
}

/// The canonical `bin/` path the infrastructure router is staged under. The
/// router launches from this staged entry at run time - like every other
/// official runtime it is resolved through the layout's flat `bin/` store.
#[must_use]
pub(crate) fn staged_router_binary(staged_root: &Path) -> PathBuf {
    staged_root.join(BIN_DIR).join(official_binary_name(
        phoxal_cli_core::project::catalog::ArtifactKind::Infrastructure,
        "router",
    ))
}

/// Complete the staged `bin/` into the loader's full required lookup store.
///
/// [`stage_participant_binary`] only stages the officials that appear as active
/// plan participants, but the loader
/// ([`required_runtimes`](phoxal_cli_core::project::layout::RuntimeLayout::required_runtimes))
/// requires *every* catalog official plus the infrastructure router: per #945
/// every official always runs, an official with no active workload simply stays
/// dormant. This materializes the remainder of that required native set - every
/// dormant catalog service and the router - into `bin/` under the same
/// canonical identity names the loader resolves against.
///
/// Entries already present (a referenced official, or a source override staged
/// by [`stage_participant_binary`]) are left in place. A source-overridden
/// dormant official is built through `build_override`; every other missing
/// official materializes via `cargo install <package>@<train> --registry
/// phoxal --locked --root <staged_root>` straight into its final `bin/` path.
///
/// `officials_source`, when set, is an already-materialized directory
/// (`<officials_source>/bin/<name>`) consulted BEFORE `cargo install`: the
/// container builder installs the always-present catalog set natively
/// inside the target-native container (see `commands::build::container`)
/// and passes its output here, so host-side staging never re-installs (or
/// cross-compiles) what the container already built correctly for the
/// target.
pub(crate) fn materialize_official_store(
    staged_root: &Path,
    resolved: &ResolvedRobot,
    offline: bool,
    officials_source: Option<&Path>,
    settings: &MaterializeSettings,
    reporter: &dyn crate::Reporter,
    mut build_override: impl FnMut(&Path, &str) -> Result<PathBuf>,
) -> Result<()> {
    let bin_dir = ensure_bin_dir(staged_root)?;
    let mut context = MaterializationContext {
        staged_root,
        bin_dir: &bin_dir,
        offline,
        officials_source,
        settings,
        reporter,
        build_override: &mut build_override,
    };
    for runtime in &resolved.platform_runtimes {
        materialize_platform_runtime(&mut context, runtime)?;
    }
    for tool in &resolved.tools {
        materialize_tool(&mut context, tool)?;
    }
    Ok(())
}

/// Materialize only the infrastructure router into `bin/`, so it launches
/// from the normal build layout like every other official. Used by the
/// Webots path, where the controller reads that same build layout; the
/// router itself is CLI-supervised identically to a native run.
pub(crate) fn stage_router_binary(
    staged_root: &Path,
    resolved: &ResolvedRobot,
    offline: bool,
    settings: &MaterializeSettings,
    reporter: &dyn crate::Reporter,
    mut build_override: impl FnMut(&Path, &str) -> Result<PathBuf>,
) -> Result<()> {
    let bin_dir = ensure_bin_dir(staged_root)?;
    let router = resolved
        .tools
        .iter()
        .find(|tool| tool.kind == phoxal_cli_core::project::catalog::ArtifactKind::Infrastructure)
        .context("resolved graph is missing the infrastructure router")?;
    let mut context = MaterializationContext {
        staged_root,
        bin_dir: &bin_dir,
        offline,
        officials_source: None,
        settings,
        reporter,
        build_override: &mut build_override,
    };
    materialize_tool(&mut context, router)
}

fn ensure_bin_dir(staged_root: &Path) -> Result<PathBuf> {
    let bin_dir = staged_root.join(BIN_DIR);
    fs::create_dir_all(&bin_dir)
        .with_context(|| format!("failed to create staged bin store {}", bin_dir.display()))?;
    Ok(bin_dir)
}

/// Look for `binary_name` already materialized under `officials_source`
/// (`<officials_source>/bin/<binary_name>`) and link it into `staged`.
///
/// When `officials_source` is `Some`, it is the container builder's own
/// `cargo install --root` output, and it is authoritative: a package this
/// staging pass expects but does not find there is the container's output
/// being INCOMPLETE, never a reason to fall back to a host-side install -
/// that fallback is precisely the host cross-compilation risk the container
/// exists to avoid, so silently taking it would defeat the whole point.
/// Returns `false` only when `officials_source` itself is `None` (no
/// container was involved in this staging pass at all), the caller's signal
/// to fall through to `cargo install`.
fn link_from_officials_source(
    officials_source: Option<&Path>,
    package: &str,
    binary_name: &str,
    staged: &Path,
) -> Result<bool> {
    let Some(source_dir) = officials_source else {
        return Ok(false);
    };
    let candidate = source_dir.join(BIN_DIR).join(binary_name);
    ensure!(
        candidate.is_file(),
        "the container did not materialize official runtime {package} (expected {}); its \
         `cargo install` output is incomplete - this is a hard error, not a fallback to a \
         host-side cross-compiled install",
        candidate.display()
    );
    link_or_copy(&candidate, staged)?;
    Ok(true)
}

/// Materialize one official platform runtime (a service or a simulator) into
/// its canonical `bin/` entry, from a source override, an already
/// -materialized `officials_source`, or `cargo install`.
struct MaterializationContext<'a> {
    staged_root: &'a Path,
    bin_dir: &'a Path,
    offline: bool,
    officials_source: Option<&'a Path>,
    settings: &'a MaterializeSettings,
    reporter: &'a dyn crate::Reporter,
    build_override: &'a mut dyn FnMut(&Path, &str) -> Result<PathBuf>,
}

fn materialize_platform_runtime(
    context: &mut MaterializationContext<'_>,
    runtime: &ResolvedPlatformRuntime,
) -> Result<()> {
    let binary_name = official_binary_name(runtime.kind, &runtime.name);
    let staged = context.bin_dir.join(&binary_name);
    if staged.is_file() {
        return Ok(());
    }
    if let Some(crate_dir) = &runtime.path_override {
        let source = (context.build_override)(crate_dir, &runtime.name)?;
        return link_or_copy(&source, &staged);
    }
    if link_from_officials_source(
        context.officials_source,
        &runtime.package,
        &binary_name,
        &staged,
    )? {
        return Ok(());
    }
    let spec = context.settings.apply(
        MaterializeSpec::new(runtime.package.clone(), runtime.train.clone())
            .with_target(runtime.target.clone()),
    );
    cargo_install(
        context.staged_root,
        &spec,
        context.offline,
        context.reporter,
    )
    .with_context(|| format!("failed to materialize official runtime {}", runtime.package))?;
    Ok(())
}

/// Materialize one official tool (or the infrastructure router) into its
/// canonical `bin/` entry, from a source override, an already-materialized
/// `officials_source`, or `cargo install`.
fn materialize_tool(context: &mut MaterializationContext<'_>, tool: &ResolvedTool) -> Result<()> {
    let staged = context.bin_dir.join(&tool.binary_name);
    if staged.is_file() {
        return Ok(());
    }
    if let Some(crate_dir) = &tool.path_override {
        let source = (context.build_override)(crate_dir, tool_participant_id(&tool.name))?;
        return link_or_copy(&source, &staged);
    }
    if link_from_officials_source(
        context.officials_source,
        &tool.package,
        &tool.binary_name,
        &staged,
    )? {
        return Ok(());
    }
    let spec = context.settings.apply(
        MaterializeSpec::new(tool.package.clone(), tool.train.clone())
            .with_target(Some(tool.target.clone())),
    );
    cargo_install(
        context.staged_root,
        &spec,
        context.offline,
        context.reporter,
    )
    .with_context(|| format!("failed to materialize official runtime {}", tool.package))?;
    Ok(())
}

/// Materialize one registry-sourced component driver package straight into
/// `bin/`, from an already-materialized `officials_source` or `cargo
/// install`, exactly like a service. A workspace/path-overridden driver is
/// staged by [`stage_participant_binary`] instead and never reaches this
/// function - see `run::stage_complete_bin_store`.
///
/// Component drivers are robot-specific (only the components a robot
/// actually declares), so the container builder cannot know them from the
/// catalog alone - it resolves the robot graph first specifically to learn
/// them, then installs them alongside the deterministic set (see
/// `commands::build::container`), so `officials_source` covers them too
/// whenever a container was involved. With no `officials_source` at all,
/// this cross-compiles host-side when `runtime.target` differs from the
/// host, with the same caveat any host cross build carries.
pub(crate) fn materialize_component_driver(
    staged_root: &Path,
    runtime: &ResolvedPlatformRuntime,
    offline: bool,
    officials_source: Option<&Path>,
    settings: &MaterializeSettings,
    reporter: &dyn crate::Reporter,
) -> Result<()> {
    let bin_dir = ensure_bin_dir(staged_root)?;
    let binary_name = official_binary_name(runtime.kind, &runtime.name);
    let staged = bin_dir.join(&binary_name);
    if staged.is_file() {
        return Ok(());
    }
    if link_from_officials_source(officials_source, &runtime.package, &binary_name, &staged)? {
        return Ok(());
    }
    let spec = settings.apply(
        MaterializeSpec::new(runtime.package.clone(), runtime.train.clone())
            .with_target(runtime.target.clone()),
    );
    cargo_install(staged_root, &spec, offline, reporter)
        .with_context(|| format!("failed to materialize component driver {}", runtime.package))?;
    Ok(())
}

/// Hardlink `source` to `dest`, falling back to a byte copy when hardlinking
/// fails (e.g. `source` and `dest` are on different filesystems). Any existing
/// `dest` is removed first so a refresh always relinks to the current bytes.
///
/// A no-op when `source` and `dest` are already the same path: `cargo
/// install --root <staged_root>` writes officials straight into their final
/// `bin/` entry, so a caller that resolves a materialized official's source
/// as its own already-staged path must not remove-then-relink it to itself.
pub(super) fn link_or_copy(source: &Path, dest: &Path) -> Result<()> {
    if source == dest {
        return Ok(());
    }
    remove_if_present(dest)?;
    if fs::hard_link(source, dest).is_ok() {
        return Ok(());
    }
    copy_binary(source, dest)
}

/// Cross-filesystem fallback for [`link_or_copy`]: a byte copy that reproduces
/// the source's executable bits so the staged entry is runnable.
pub(super) fn copy_binary(source: &Path, dest: &Path) -> Result<()> {
    fs::copy(source, dest).with_context(|| {
        format!(
            "failed to stage binary {} into {}",
            source.display(),
            dest.display()
        )
    })?;
    let permissions = fs::metadata(source)
        .with_context(|| format!("failed to stat staged binary source {}", source.display()))?
        .permissions();
    fs::set_permissions(dest, permissions)
        .with_context(|| format!("failed to set staged binary mode on {}", dest.display()))?;
    Ok(())
}
