//! Participant materialization and flat binary-store staging.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use phoxal_cli_core::project::launch_plan::ParticipantLaunchRecord;
use phoxal_cli_core::project::resolver::{
    BundlePlan, ResolvedPlatformRuntime, official_binary_name,
};

use super::publish::remove_if_present;
use crate::build::materialise::{MaterializeProfile, MaterializeSpec, cargo_install_batch};

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
/// the compiler-owned participant declarations - so this is just that name.
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
/// outside the plan/layout contract entirely and runs in place, never here.
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

/// Complete one unpublished candidate's registry/source-override store in a
/// single collection context. Platform entries and selected
/// registry drivers share the same pending install groups before Cargo runs.
#[allow(clippy::too_many_arguments)]
pub(crate) fn materialize_candidate_store(
    staged_root: &Path,
    resolved: &BundlePlan,
    extra_registry_runtimes: &[&ResolvedPlatformRuntime],
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
        pending: Vec::new(),
    };
    for runtime in &resolved.platform_runtimes {
        materialize_platform_runtime(&mut context, runtime)?;
    }
    for runtime in extra_registry_runtimes {
        queue_component_driver(&mut context, runtime)?;
    }
    context.install_pending()
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
    pending: Vec<MaterializeSpec>,
}

impl MaterializationContext<'_> {
    fn install_pending(&mut self) -> Result<()> {
        let mut groups = BTreeMap::<
            (Option<String>, MaterializeProfile, Option<PathBuf>),
            Vec<MaterializeSpec>,
        >::new();
        for spec in std::mem::take(&mut self.pending) {
            groups
                .entry((spec.target.clone(), spec.profile, spec.target_dir.clone()))
                .or_default()
                .push(spec);
        }
        let total = groups.len();
        for (index, specs) in groups.into_values().enumerate() {
            let packages = specs
                .iter()
                .map(crate::build::materialise::MaterializeSpec::cargo_package_name)
                .collect::<std::collections::BTreeSet<_>>()
                .len();
            self.reporter
                .report(crate::PreparationEvent::RegistryInstallGroupStarted {
                    current: index + 1,
                    total,
                    packages,
                });
            if let Err(error) =
                cargo_install_batch(self.staged_root, &specs, self.offline, self.reporter)
            {
                self.reporter
                    .report(crate::PreparationEvent::RegistryInstallGroupFinished {
                        current: index + 1,
                        total,
                        success: false,
                    });
                return Err(error);
            }
            self.reporter
                .report(crate::PreparationEvent::RegistryInstallGroupFinished {
                    current: index + 1,
                    total,
                    success: true,
                });
        }
        Ok(())
    }
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
    context.pending.push(spec);
    Ok(())
}

fn queue_component_driver(
    context: &mut MaterializationContext<'_>,
    runtime: &ResolvedPlatformRuntime,
) -> Result<()> {
    let binary_name = official_binary_name(runtime.kind, &runtime.name);
    let staged = context.bin_dir.join(&binary_name);
    if staged.is_file()
        || link_from_officials_source(
            context.officials_source,
            &runtime.package,
            &binary_name,
            &staged,
        )?
    {
        return Ok(());
    }
    context.pending.push(
        context.settings.apply(
            MaterializeSpec::new(runtime.package.clone(), runtime.train.clone())
                .with_target(runtime.target.clone()),
        ),
    );
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
