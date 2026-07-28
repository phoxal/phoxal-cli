//! The unified runtime-layout stager.
//!
//! One stager materializes a source project into the runtime layout at
//! `.phoxal/bundle/` (organization#951 WS4 - no per-triple nesting, since one
//! robot targets one platform at a time):
//!
//! ```text
//! robot.yaml    # fully flattened, resolved robot/v0 with the complete service map
//! bin/          # flat binary lookup store, populated by `cargo install --root`
//!               # (officials) and hardlinks/copies (workspace-built binaries)
//! model/        # structure/URDF assets, when referenced
//! components/   # compiled runtime component assets
//! behaviors/    # when referenced
//! ```
//!
//! The compiled `robot.yaml` + assets swap is atomic per refresh (stage into a
//! sibling temp dir, then rename), so a crashed pass never leaves a
//! half-written layout. `bin/` is (re)populated from official packages and
//! resolved binaries every refresh so it can never go stale. `cargo install
//! --root <candidate>` targets the SAME candidate directory the robot.yaml
//! and assets are staged into, so its `bin/` entries land directly at their
//! final path with no separate harvest-then-link step; `cargo install` also
//! leaves `.crates.toml`/`.crates2.json` dotfiles in the candidate root,
//! which the archiver (`phoxal build`) excludes rather than fighting
//! `--no-track` (organization#951 WS4: `--no-track` disables Cargo's own
//! concurrent-invocation protection for no real benefit here).
//!
//! The live `.phoxal/bundle/` is never deleted before every install and
//! validation succeeds: staging always builds into a sibling candidate
//! directory first, validates it, and only then atomically renames it over
//! the previous complete layout. A build failure halfway through must never
//! leave a robot with no runtime.
//!
//! This is why [`begin_runtime_layout`]/[`publish_runtime_layout`] are two
//! functions, not one: everything between them - `cargo install` for every
//! official, building every source/override binary, the source check, and
//! the loader's own execution-time validation - runs against
//! [`StagedCandidate::path`], a path nobody executes from yet. Only
//! [`publish_runtime_layout`] ever touches the live path, and it is always
//! the last call. [`stage_runtime_layout`] is the two collapsed into one for
//! a caller that never populates `bin/`; a caller that does MUST use the
//! split functions, not it - see `run::prepare::refresh_staging` and
//! `simulation::setup::live_simulate_setup`.

use std::fs;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, ensure};

use phoxal_cli_core::project::launch_plan::{ParticipantLaunchRecord, runtime_layout_dir};
use phoxal_cli_core::project::resolver::{
    ResolvedPlatformRuntime, ResolvedRobot, ResolvedTool, official_binary_name, tool_participant_id,
};

use crate::materialize::{MaterializeProfile, MaterializeSpec, cargo_install};

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

const PREVIOUS_LAYOUT_SUFFIX: &str = ".previous";
const BEHAVIORS_DIR: &str = "behaviors";
const MESHES_DIR: &str = "meshes";
const BIN_DIR: &str = "bin";
const COMPONENT_FILE: &str = "component.yaml";
const COMPONENT_OPTIONAL_FILES: [&str; 2] = ["structure.urdf", "simulation.yaml"];

/// The staged runtime layout directory for this resolved robot under
/// `project_root`. `run`, live simulation, and `build` all stage and execute
/// this one root.
#[must_use]
pub fn layout_path(project_root: &Path, _resolved: &ResolvedRobot) -> PathBuf {
    runtime_layout_dir(project_root)
}

/// An unpublished runtime-layout candidate: the compiled `robot.yaml` and
/// runtime assets already staged (and manifest-shape validated), but NOT yet
/// swapped into the live `.phoxal/bundle/`.
///
/// This is the type that makes the stager's atomicity promise real: every
/// caller MUST materialize officials ([`materialize_official_store`]),
/// build/stage source and override binaries, run its source/loader
/// validation, and only THEN call [`publish_runtime_layout`]. A failure at
/// any point before publish touches only this candidate's own temporary
/// directory - the previous live bundle is untouched and still runnable.
/// [`StagedCandidate::path`] is a plain filesystem path, so every existing
/// staging function (`materialize_official_store`, `stage_named_binary`,
/// `crate::loader::validate_layout_plan`, ...) that takes `staged_root: &Path`
/// already works against it with no changes.
pub struct StagedCandidate {
    dir: tempfile::TempDir,
    project_root: PathBuf,
}

impl StagedCandidate {
    #[must_use]
    pub fn path(&self) -> &Path {
        self.dir.path()
    }
}

/// Stage the compiled `robot.yaml` and runtime assets into a fresh,
/// UNPUBLISHED candidate directory - a sibling of the live `.phoxal/bundle/`,
/// never the live path itself. The caller owns the project run lock for the
/// whole operation (candidate creation through [`publish_runtime_layout`]),
/// so no participant observes anything until the single atomic rename at the
/// end. `bin/` is created empty; the caller populates it (typically with
/// [`materialize_official_store`] and [`stage_participant_binary`]) and runs
/// its own validation against [`StagedCandidate::path`] BEFORE publishing -
/// see the module docs.
pub fn begin_runtime_layout(
    project_root: &Path,
    resolved: &ResolvedRobot,
) -> Result<StagedCandidate> {
    let build_dir =
        project_root.join(phoxal_cli_core::project::launch_plan::RUNTIME_BUNDLE_ROOT_RELATIVE);
    let parent = build_dir
        .parent()
        .context("runtime bundle directory has no parent")?;
    fs::create_dir_all(parent).with_context(|| {
        format!(
            "failed to create runtime layout directory {}",
            parent.display()
        )
    })?;
    let candidate = tempfile::Builder::new()
        .prefix(".bundle-candidate-")
        .tempdir_in(parent)
        .with_context(|| {
            format!(
                "failed to create runtime layout candidate in {}",
                parent.display()
            )
        })?;

    let compiled = compile_manifest(resolved);
    stage_candidate(project_root, candidate.path(), resolved, &compiled)?;
    validate_candidate(candidate.path(), resolved, &compiled)?;

    Ok(StagedCandidate {
        dir: candidate,
        project_root: project_root.to_path_buf(),
    })
}

/// Atomically publish `candidate` as the live `.phoxal/bundle/`, replacing
/// any previous layout. Call this ONLY after every install, source build,
/// metadata read, and loader validation against `candidate.path()` has
/// already succeeded - this is the exact promise the module docs make, and
/// the only step allowed to touch the live path.
pub fn publish_runtime_layout(
    candidate: StagedCandidate,
    resolved: &ResolvedRobot,
) -> Result<PathBuf> {
    let StagedCandidate {
        dir: candidate,
        project_root,
    } = candidate;
    let target = layout_path(&project_root, resolved);
    let parent = target
        .parent()
        .context("runtime bundle directory has no parent")?;
    let previous = parent.join(format!(".bundle{PREVIOUS_LAYOUT_SUFFIX}"));
    remove_if_present(&previous)?;
    let candidate = candidate.keep();
    let had_previous = fs::symlink_metadata(&target).is_ok();
    if had_previous {
        fs::rename(&target, &previous).with_context(|| {
            format!(
                "failed to move previous runtime layout {} aside",
                target.display()
            )
        })?;
    }

    if let Err(error) = fs::rename(&candidate, &target) {
        if had_previous {
            let _ = fs::rename(&previous, &target);
        }
        let _ = remove_if_present(&candidate);
        return Err(error).with_context(|| {
            format!(
                "failed to atomically publish runtime layout {}",
                target.display()
            )
        });
    }
    remove_if_present(&previous)?;
    Ok(target)
}

/// Stage the compiled `robot.yaml` and runtime assets into `.phoxal/bundle/`
/// and publish immediately, with no materialization in between - the
/// [`begin_runtime_layout`] + [`publish_runtime_layout`] pair collapsed into
/// one call. Correct ONLY for a caller that never populates `bin/` (a bare
/// manifest/assets refresh); every REAL caller installs officials or builds
/// binaries, and MUST use the split functions directly instead, publishing
/// only after that work and its validation succeed - see
/// `refresh_staging`/`live_simulate_setup`. `#[cfg(test)]`-only: production
/// code has no bin/-free staging pass left to use it for.
#[cfg(test)]
pub(crate) fn stage_runtime_layout(
    project_root: &Path,
    resolved: &ResolvedRobot,
) -> Result<PathBuf> {
    let candidate = begin_runtime_layout(project_root, resolved)?;
    publish_runtime_layout(candidate, resolved)
}

/// The compiled `robot/v0` manifest for the staged layout. Under the
/// declaration model (#950) the authored `services:` and `tools:` maps are
/// already complete - they select which discovered workspace runtimes belong
/// to the robot - so compilation carries them verbatim (the `extends:` chain
/// was already flattened by the framework loader); nothing is injected from
/// discovery.
fn compile_manifest(resolved: &ResolvedRobot) -> phoxal::model::robot::v0::Robot {
    resolved.robot.clone()
}

/// The canonical identity name one launched participant is stored under in
/// `bin/`. The source-free plan (#936) names each participant's `bin/` binary
/// on its `execution` directly - the loader resolves the identical name from
/// the compiled `robot.yaml` - so this is just that name.
fn canonical_binary_name(participant: &ParticipantLaunchRecord) -> String {
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
pub fn staged_router_binary(staged_root: &Path) -> PathBuf {
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
pub fn materialize_official_store(
    staged_root: &Path,
    resolved: &ResolvedRobot,
    offline: bool,
    officials_source: Option<&Path>,
    settings: &MaterializeSettings,
    mut build_override: impl FnMut(&Path, &str) -> Result<PathBuf>,
) -> Result<()> {
    let bin_dir = ensure_bin_dir(staged_root)?;
    for runtime in &resolved.platform_runtimes {
        materialize_platform_runtime(
            staged_root,
            &bin_dir,
            runtime,
            offline,
            officials_source,
            settings,
            &mut build_override,
        )?;
    }
    for tool in &resolved.tools {
        materialize_tool(
            staged_root,
            &bin_dir,
            tool,
            offline,
            officials_source,
            settings,
            &mut build_override,
        )?;
    }
    Ok(())
}

/// Materialize only the infrastructure router into `bin/`, so it launches
/// from the normal build layout like every other official. Used by the
/// Webots path, where the controller reads that same build layout; the
/// router itself is CLI-supervised identically to a native run.
pub fn stage_router_binary(
    staged_root: &Path,
    resolved: &ResolvedRobot,
    offline: bool,
    settings: &MaterializeSettings,
    mut build_override: impl FnMut(&Path, &str) -> Result<PathBuf>,
) -> Result<()> {
    let bin_dir = ensure_bin_dir(staged_root)?;
    let router = resolved
        .tools
        .iter()
        .find(|tool| tool.kind == phoxal_cli_core::project::catalog::ArtifactKind::Infrastructure)
        .context("resolved graph is missing the infrastructure router")?;
    materialize_tool(
        staged_root,
        &bin_dir,
        router,
        offline,
        None,
        settings,
        &mut build_override,
    )
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
fn materialize_platform_runtime(
    staged_root: &Path,
    bin_dir: &Path,
    runtime: &ResolvedPlatformRuntime,
    offline: bool,
    officials_source: Option<&Path>,
    settings: &MaterializeSettings,
    build_override: &mut impl FnMut(&Path, &str) -> Result<PathBuf>,
) -> Result<()> {
    let binary_name = official_binary_name(runtime.kind, &runtime.name);
    let staged = bin_dir.join(&binary_name);
    if staged.is_file() {
        return Ok(());
    }
    if let Some(crate_dir) = &runtime.path_override {
        let source = build_override(crate_dir, &runtime.name)?;
        return link_or_copy(&source, &staged);
    }
    if link_from_officials_source(officials_source, &runtime.package, &binary_name, &staged)? {
        return Ok(());
    }
    let spec = settings.apply(
        MaterializeSpec::new(runtime.package.clone(), runtime.train.clone())
            .with_target(runtime.target.clone()),
    );
    cargo_install(staged_root, &spec, offline)
        .with_context(|| format!("failed to materialize official runtime {}", runtime.package))?;
    Ok(())
}

/// Materialize one official tool (or the infrastructure router) into its
/// canonical `bin/` entry, from a source override, an already-materialized
/// `officials_source`, or `cargo install`.
fn materialize_tool(
    staged_root: &Path,
    bin_dir: &Path,
    tool: &ResolvedTool,
    offline: bool,
    officials_source: Option<&Path>,
    settings: &MaterializeSettings,
    build_override: &mut impl FnMut(&Path, &str) -> Result<PathBuf>,
) -> Result<()> {
    let staged = bin_dir.join(&tool.binary_name);
    if staged.is_file() {
        return Ok(());
    }
    if let Some(crate_dir) = &tool.path_override {
        let source = build_override(crate_dir, tool_participant_id(&tool.name))?;
        return link_or_copy(&source, &staged);
    }
    if link_from_officials_source(officials_source, &tool.package, &tool.binary_name, &staged)? {
        return Ok(());
    }
    let spec = settings.apply(
        MaterializeSpec::new(tool.package.clone(), tool.train.clone())
            .with_target(Some(tool.target.clone())),
    );
    cargo_install(staged_root, &spec, offline)
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
pub fn materialize_component_driver(
    staged_root: &Path,
    runtime: &ResolvedPlatformRuntime,
    offline: bool,
    officials_source: Option<&Path>,
    settings: &MaterializeSettings,
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
    cargo_install(staged_root, &spec, offline)
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
fn link_or_copy(source: &Path, dest: &Path) -> Result<()> {
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
fn copy_binary(source: &Path, dest: &Path) -> Result<()> {
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

fn stage_candidate(
    project_root: &Path,
    candidate: &Path,
    resolved: &ResolvedRobot,
    compiled: &phoxal::model::robot::v0::Robot,
) -> Result<()> {
    phoxal::model::robot::Robot::V0(compiled.clone())
        .write_to_dir(candidate)
        .context("failed to write compiled runtime robot.yaml")?;
    crate::runtime_header::RuntimeHeader::for_phoxal_version(&resolved.train)
        .write_to(candidate)
        .context("failed to write compiled runtime compatibility header")?;

    let structure = &resolved.robot.robot.structure;
    ensure_safe_relative_path(structure, "robot structure")?;
    copy_file_preserving_path(project_root, candidate, structure, "robot structure")?;
    if let Some(structure_parent) = structure.parent() {
        let mesh_path = structure_parent.join(MESHES_DIR);
        copy_optional_dir_preserving_path(project_root, candidate, &mesh_path)?;
    }
    copy_optional_dir_preserving_path(project_root, candidate, Path::new(BEHAVIORS_DIR))?;

    // The router's optional Zenoh config file is a real runtime asset: stage it
    // into the layout under its runtime-root-relative path so a `build.phoxal`
    // extracted anywhere resolves the same relative path a source run does
    // (#936, finding 4). It must be a safe relative path with no escapes, exactly
    // like the robot structure.
    if let Some(router_config) = &resolved.robot.router.config {
        ensure_safe_relative_path(router_config, "router.config")?;
        copy_file_preserving_path(project_root, candidate, router_config, "router.config")?;
    }

    stage_component_bundles(candidate, resolved)
        .context("failed to stage component assets into the runtime layout")
}

/// Copy every distinct component's asset bundle (`component.yaml`, the
/// optional `structure.urdf`/`simulation.yaml`, and `meshes/`) into the
/// staged layout, from whichever on-disk directory resolution already
/// settled for it - a workspace crate directory or a registry package's
/// extraction directory `cargo metadata` reported. Both sources are read the
/// same way: resolution is the only place that knows which is which.
fn stage_component_bundles(candidate: &Path, resolved: &ResolvedRobot) -> Result<()> {
    let mut staged = std::collections::BTreeSet::new();
    for component in &resolved.components {
        let component_id = &component.source_name;
        if !staged.insert(component_id.clone()) {
            continue;
        }
        let source_dir = component
            .assets
            .path_override()
            .with_context(|| format!("failed to locate component assets for '{component_id}'"))?;
        // Schema gate every referenced component document before it is
        // staged: verify the declared `component/vX` revision this CLI
        // supports, then strict-parse it, so an unknown field or unsupported
        // revision fails here - naming the exact file - instead of being
        // copied through silently (#936, finding 5).
        gate_component_document(source_dir, component_id)?;
        let dest_dir = candidate.join("components").join(component_id);
        if source_dir == dest_dir {
            continue;
        }
        copy_component_bundle_files(source_dir, &dest_dir)?;
    }
    Ok(())
}

fn gate_component_document(source_dir: &Path, component_id: &str) -> Result<()> {
    let component_file = source_dir.join(COMPONENT_FILE);
    phoxal_cli_core::schema::ensure_supported_revision(
        &component_file,
        phoxal_cli_core::schema::DocumentKind::Component,
    )?;
    phoxal::model::component::Component::read_from_dir(source_dir).with_context(|| {
        format!(
            "component '{component_id}' failed strict parsing of {}",
            component_file.display()
        )
    })?;
    Ok(())
}

fn copy_component_bundle_files(source_dir: &Path, dest_dir: &Path) -> Result<()> {
    fs::create_dir_all(dest_dir)
        .with_context(|| format!("failed to create {}", dest_dir.display()))?;

    let component_file = source_dir.join(COMPONENT_FILE);
    fs::copy(&component_file, dest_dir.join(COMPONENT_FILE)).with_context(|| {
        format!(
            "failed to stage component metadata {} to {}",
            component_file.display(),
            dest_dir.display()
        )
    })?;

    for optional_file in COMPONENT_OPTIONAL_FILES {
        let source_file = source_dir.join(optional_file);
        if !source_file.is_file() {
            continue;
        }
        fs::copy(&source_file, dest_dir.join(optional_file)).with_context(|| {
            format!(
                "failed to stage {} to {}",
                source_file.display(),
                dest_dir.display()
            )
        })?;
    }

    let meshes_source = source_dir.join(MESHES_DIR);
    if meshes_source.is_dir() {
        copy_dir_recursive(&meshes_source, &dest_dir.join(MESHES_DIR))?;
    }
    Ok(())
}

fn ensure_safe_relative_path(path: &Path, label: &str) -> Result<()> {
    ensure!(
        !path.as_os_str().is_empty()
            && path
                .components()
                .all(|component| matches!(component, Component::Normal(_))),
        "{label} must be a non-empty relative path without '.' or '..': {}",
        path.display()
    );
    Ok(())
}

fn validate_candidate(
    candidate: &Path,
    resolved: &ResolvedRobot,
    compiled: &phoxal::model::robot::v0::Robot,
) -> Result<()> {
    // Resolution already ran the model's semantic validation. Reparse the
    // serialized candidate here to prove the on-disk manifest is complete
    // and strict without losing that owner-specific validation context.
    let staged = phoxal::model::robot::Robot::parse_from_dir(candidate)
        .context("compiled runtime robot.yaml failed strict parsing")?;
    ensure!(
        staged.as_v0() == compiled,
        "compiled runtime robot.yaml differs from the resolved manifest"
    );
    ensure!(
        candidate.join(&resolved.robot.robot.structure).is_file(),
        "compiled runtime layout is missing robot structure {}",
        resolved.robot.robot.structure.display()
    );

    for component in &resolved.components {
        {
            let component_file = candidate
                .join("components")
                .join(&component.source_name)
                .join("component.yaml");
            ensure!(
                component_file.is_file(),
                "compiled runtime layout is missing component metadata {}",
                component_file.display()
            );
        }
    }
    Ok(())
}

fn copy_file_preserving_path(
    project_root: &Path,
    candidate: &Path,
    relative: &Path,
    label: &str,
) -> Result<()> {
    let source = project_root.join(relative);
    let dest = candidate.join(relative);
    let parent = dest
        .parent()
        .context("runtime layout destination has no parent")?;
    fs::create_dir_all(parent).with_context(|| format!("failed to create {}", parent.display()))?;
    fs::copy(&source, &dest).with_context(|| {
        format!(
            "failed to stage {label} {} to {}",
            source.display(),
            dest.display()
        )
    })?;
    Ok(())
}

fn copy_optional_dir_preserving_path(
    project_root: &Path,
    candidate: &Path,
    relative: &Path,
) -> Result<()> {
    let source = project_root.join(relative);
    if !source.is_dir() {
        return Ok(());
    }
    copy_dir_recursive(&source, &candidate.join(relative))
}

/// Copy a directory tree into `dest` (files and directories; permissions
/// preserved by `fs::copy`). Shared with `phoxal build`'s container path, which
/// publishes the snapshot-staged layout into the real project (#936).
pub(crate) fn copy_tree_into(source: &Path, dest: &Path) -> Result<()> {
    copy_dir_recursive(source, dest)
}

fn copy_dir_recursive(source: &Path, dest: &Path) -> Result<()> {
    fs::create_dir_all(dest).with_context(|| format!("failed to create {}", dest.display()))?;
    for entry in
        fs::read_dir(source).with_context(|| format!("failed to read {}", source.display()))?
    {
        let entry = entry?;
        let source_path = entry.path();
        let dest_path = dest.join(entry.file_name());
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            copy_dir_recursive(&source_path, &dest_path)?;
        } else if file_type.is_file() {
            fs::copy(&source_path, &dest_path).with_context(|| {
                format!(
                    "failed to stage runtime asset {} to {}",
                    source_path.display(),
                    dest_path.display()
                )
            })?;
        }
    }
    Ok(())
}

fn remove_if_present(path: &Path) -> Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to inspect {}", path.display()));
        }
    };
    if metadata.is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
    .with_context(|| format!("failed to remove stale runtime state {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host_paths::test_support::ScratchPhoxalHome;
    use crate::resolver::host_target_triple;
    use phoxal_cli_core::identity::{ExecutionId, ProducerId};
    use phoxal_cli_core::project::catalog::ArtifactKind;
    use phoxal_cli_core::project::launch_plan::ParticipantExecution;
    use phoxal_cli_core::project::resolver::{
        ResolvedComponent, ResolvedComponentPackage, ResolvedComponentSource, ResolvedRobot,
        ResolvedUserRuntime,
    };
    use std::os::unix::fs::MetadataExt;

    fn resolved_robot() -> Result<ResolvedRobot> {
        let yaml = r#"schema: robot/v0
robot:
  id: testbot
  namespace: dev
  motion_limits:
    max_linear_speed_mps: 0.6
    max_angular_speed_radps: 2.0
  structure: model/structure.urdf
  kinematic:
    kind: omnidirectional
    actuators: []
    encoders: []
  components: {}
"#;
        Ok(ResolvedRobot {
            robot: phoxal::model::robot::v0::Robot::parse_from_string(yaml)?,
            train: "0.36.0".to_string(),
            target: host_target_triple(),
            platform_runtimes: Vec::new(),
            simulators: Vec::new(),
            user_runtimes: Vec::new(),
            user_tools: Vec::new(),
            undeclared_runtimes: Vec::new(),
            components: Vec::new(),
            tools: Vec::new(),
            path_overrides: Vec::new(),
        })
    }

    fn launch_record(
        participant_id: &str,
        artifact_id: &str,
        execution: ParticipantExecution,
        component_instance: Option<&str>,
    ) -> ParticipantLaunchRecord {
        use phoxal::participant::launch::{
            BusProfile, ClockMode, DEFAULT_SHUTDOWN_GRACE_MS, ParticipantLaunch,
        };
        ParticipantLaunchRecord {
            artifact_id: artifact_id.to_string(),
            execution,
            launch: ParticipantLaunch {
                participant_id: participant_id.to_string(),
                execution: ExecutionId::mint(),
                producer: ProducerId::mint(),
                execution_origin: None,
                namespace: "dev".to_string(),
                robot_id: "testbot".to_string(),
                bus: BusProfile {
                    connect_endpoints: Vec::new(),
                },
                clock: ClockMode::Real,
                config: None,
                robot_root: None,
                component_instance: component_instance.map(str::to_string),
                shutdown_grace_ms: DEFAULT_SHUTDOWN_GRACE_MS,
            },
            startup_requirement: phoxal_cli_core::session::StartupRequirement::Required,
            runtime_failure: phoxal_cli_core::session::RuntimeFailurePolicy::StopProject,
        }
    }

    #[test]
    fn stages_the_full_layout_without_cargo_or_source_or_dotphoxal() -> Result<()> {
        let _scratch = ScratchPhoxalHome::new()?;
        let project = tempfile::tempdir()?;
        fs::create_dir_all(project.path().join("model/meshes"))?;
        fs::create_dir_all(project.path().join(BEHAVIORS_DIR))?;
        fs::write(project.path().join("model/structure.urdf"), "<robot/>")?;
        fs::write(project.path().join("model/meshes/chassis.dae"), "mesh")?;
        fs::write(
            project.path().join("behaviors/default.yaml"),
            "behavior: []",
        )?;
        // Source-project noise that must never appear in the staged layout.
        fs::write(project.path().join("Cargo.toml"), "[workspace]\n")?;
        fs::write(project.path().join("lib.rs"), "fn main() {}\n")?;

        let resolved = resolved_robot()?;
        let staged = stage_runtime_layout(project.path(), &resolved)?;
        assert_eq!(staged, layout_path(project.path(), &resolved));
        assert!(staged.starts_with(project.path().join(".phoxal/bundle")));

        assert!(staged.join("robot.yaml").is_file());
        assert_eq!(
            fs::read_to_string(staged.join("model/structure.urdf"))?,
            "<robot/>"
        );
        assert!(staged.join("model/meshes/chassis.dae").is_file());
        assert!(staged.join("behaviors/default.yaml").is_file());

        // No source, no Cargo manifests, no nested `.phoxal` in the layout.
        assert!(!staged.join("Cargo.toml").exists());
        assert!(!staged.join("lib.rs").exists());
        assert!(!staged.join(".phoxal").exists());

        // Re-stage replaces the previous generation atomically (no leftovers).
        fs::write(staged.join("stale"), "old")?;
        stage_runtime_layout(project.path(), &resolved)?;
        assert!(!staged.join("stale").exists());
        assert!(!project.path().join(".phoxal/bundle.previous").exists());
        Ok(())
    }

    #[test]
    fn compiled_manifest_carries_the_authored_declarations_verbatim() -> Result<()> {
        let _scratch = ScratchPhoxalHome::new()?;
        let project = tempfile::tempdir()?;
        fs::create_dir_all(project.path().join("model"))?;
        fs::write(project.path().join("model/structure.urdf"), "<robot/>")?;

        let mut resolved = resolved_robot()?;
        // The declaration model (#950): the authored maps are complete - one
        // declared service (selected), one declared tool, and one discovered
        // crate that is NOT declared and therefore never enters the compiled
        // document.
        resolved.robot.services.insert(
            "mission".to_string(),
            phoxal::model::robot::v0::UserService {
                config: Some(serde_json::json!({"speed": 1})),
            },
        );
        resolved.robot.tools.insert(
            "lidar-viz".to_string(),
            phoxal::model::robot::v0::UserTool {
                config: Some(serde_json::json!({"port": 9000})),
            },
        );
        resolved.user_runtimes.push(ResolvedUserRuntime {
            name: "mission".to_string(),
            path: PathBuf::from("services/mission"),
            source_hash: "hash".to_string(),
        });
        resolved.user_tools.push(ResolvedUserRuntime {
            name: "lidar-viz".to_string(),
            path: PathBuf::from("tools/lidar-viz"),
            source_hash: "hash".to_string(),
        });
        resolved
            .undeclared_runtimes
            .push(phoxal_cli_core::project::resolver::UndeclaredRuntime {
                name: "telemetry".to_string(),
                family: "services",
            });

        let staged = stage_runtime_layout(project.path(), &resolved)?;
        let compiled = phoxal::model::robot::Robot::parse_from_dir(&staged)?
            .as_v0()
            .clone();
        // The compiled document is the authored declarations verbatim: the
        // undeclared discovered crate is absent, nothing is injected.
        assert_eq!(
            compiled.services.keys().collect::<Vec<_>>(),
            vec!["mission"]
        );
        assert_eq!(
            compiled.services["mission"].config,
            Some(serde_json::json!({"speed": 1}))
        );
        assert_eq!(compiled.tools.keys().collect::<Vec<_>>(), vec!["lidar-viz"]);
        assert_eq!(
            compiled.tools["lidar-viz"].config,
            Some(serde_json::json!({"port": 9000}))
        );
        Ok(())
    }

    /// The router's `router.config` file is staged into the layout under its
    /// relative path and resolves inside the layout in both a source run and an
    /// extracted bundle (#936, finding 4): staging copies it, and simulating an
    /// extraction (moving the layout elsewhere) still resolves it inside the
    /// moved layout.
    #[test]
    fn router_config_is_staged_into_the_layout_and_resolves_after_extraction() -> Result<()> {
        let _scratch = ScratchPhoxalHome::new()?;
        let project = tempfile::tempdir()?;
        fs::create_dir_all(project.path().join("model"))?;
        fs::create_dir_all(project.path().join("config"))?;
        fs::write(project.path().join("model/structure.urdf"), "<robot/>")?;
        fs::write(
            project.path().join("config/zenoh.json5"),
            "{ mode: \"router\" }",
        )?;

        let mut resolved = resolved_robot()?;
        resolved.robot.router.config = Some(PathBuf::from("config/zenoh.json5"));
        let staged = stage_runtime_layout(project.path(), &resolved)?;

        // The config landed inside the layout under its relative path.
        assert_eq!(
            fs::read_to_string(staged.join("config/zenoh.json5"))?,
            "{ mode: \"router\" }"
        );
        // A source run resolves it inside the staged layout.
        assert_eq!(
            crate::run::resolve_router_config(&resolved.robot, &staged)?,
            Some(staged.join("config/zenoh.json5"))
        );

        // Simulate extracting a `build.phoxal`: move the layout to a fresh root
        // (the source tree is gone). Resolution must still succeed inside it.
        let extracted = tempfile::tempdir()?;
        let extracted_root = extracted.path().join("layout");
        copy_tree(&staged, &extracted_root)?;
        assert_eq!(
            crate::run::resolve_router_config(&resolved.robot, &extracted_root)?,
            Some(extracted_root.join("config/zenoh.json5"))
        );
        Ok(())
    }

    /// A `router.config` that escapes the runtime layout is rejected at staging,
    /// exactly like an escaping robot structure (#936, finding 4).
    #[test]
    fn router_config_cannot_escape_the_runtime_layout() -> Result<()> {
        let _scratch = ScratchPhoxalHome::new()?;
        let project = tempfile::tempdir()?;
        fs::create_dir_all(project.path().join("model"))?;
        fs::write(project.path().join("model/structure.urdf"), "<robot/>")?;
        let mut resolved = resolved_robot()?;
        resolved.robot.router.config = Some(PathBuf::from("../outside.json5"));
        let error = stage_runtime_layout(project.path(), &resolved)
            .unwrap_err()
            .to_string();
        assert!(error.contains("router.config"), "{error}");
        Ok(())
    }

    /// A small recursive copy for the extraction simulation above.
    fn copy_tree(source: &Path, dest: &Path) -> Result<()> {
        fs::create_dir_all(dest)?;
        for entry in fs::read_dir(source)? {
            let entry = entry?;
            let target = dest.join(entry.file_name());
            if entry.file_type()?.is_dir() {
                copy_tree(&entry.path(), &target)?;
            } else {
                fs::copy(entry.path(), &target)?;
            }
        }
        Ok(())
    }

    #[test]
    fn robot_structure_cannot_escape_the_runtime_layout() -> Result<()> {
        let project = tempfile::tempdir()?;
        for structure in [
            PathBuf::from("../outside.urdf"),
            PathBuf::from("/tmp/outside.urdf"),
        ] {
            let mut resolved = resolved_robot()?;
            resolved.robot.robot.structure = structure.clone();
            let error = stage_runtime_layout(project.path(), &resolved)
                .unwrap_err()
                .to_string();
            assert!(error.contains("robot structure must be a non-empty relative path"));
            assert!(!layout_path(project.path(), &resolved).exists());

            let build_dir = project.path().join(".phoxal");
            let candidates = fs::read_dir(&build_dir)?
                .filter_map(std::result::Result::ok)
                .filter(|entry| entry.file_name().to_string_lossy().contains("-candidate-"))
                .count();
            assert_eq!(candidates, 0, "failed candidates must clean themselves up");
        }
        Ok(())
    }

    #[test]
    fn failed_candidate_preserves_previous_layout() -> Result<()> {
        let _scratch = ScratchPhoxalHome::new()?;
        let project = tempfile::tempdir()?;
        fs::create_dir_all(project.path().join("model"))?;
        fs::write(project.path().join("model/structure.urdf"), "first")?;
        let resolved = resolved_robot()?;
        let staged = stage_runtime_layout(project.path(), &resolved)?;

        fs::remove_file(project.path().join("model/structure.urdf"))?;
        assert!(stage_runtime_layout(project.path(), &resolved).is_err());
        assert_eq!(
            fs::read_to_string(staged.join("model/structure.urdf"))?,
            "first"
        );
        Ok(())
    }

    /// The atomicity promise the module docs make, exercised past the point
    /// [`failed_candidate_preserves_previous_layout`] stops at: a failure
    /// during binary MATERIALIZATION (`materialize_official_store`, after
    /// `begin_runtime_layout` already succeeded) must never touch the
    /// previously published, running bundle. Uses the `officials_source`
    /// hard-error path (organization#951 WS4 review, blocker 2) as the
    /// deterministic, offline-safe failure trigger: a container-shaped
    /// directory whose `bin/` is missing the expected official.
    ///
    /// "...and_runnable" is proven literally (organization#951 WS4 review,
    /// round 2 nitpick): the marker is a real executable shell script, run
    /// via `std::process::Command` both before and after the failed second
    /// refresh, not just read as bytes - a test named "runnable" that only
    /// ever read the file would claim more than it proved.
    #[test]
    fn materialization_failure_leaves_previous_bundle_intact_and_runnable() -> Result<()> {
        use std::os::unix::fs::PermissionsExt;

        let _scratch = ScratchPhoxalHome::new()?;
        let project = tempfile::tempdir()?;
        fs::create_dir_all(project.path().join("model"))?;
        fs::write(project.path().join("model/structure.urdf"), "<robot/>")?;
        let mut resolved = resolved_robot()?;
        resolved.platform_runtimes = vec![platform_runtime("mission", ArtifactKind::Service)];
        let binary_name = official_binary_name(ArtifactKind::Service, "mission");

        // First refresh: a complete `officials_source` (the shape the
        // container builder hands to host-side staging) materializes and
        // publishes cleanly. The marker is a real, executable shell script -
        // not just bytes - so the "runnable" half of this test's name is
        // literally exercised below, not merely asserted by content.
        let complete_officials = tempfile::tempdir()?;
        fs::create_dir_all(complete_officials.path().join("bin"))?;
        let marker_script = complete_officials.path().join("bin").join(&binary_name);
        fs::write(&marker_script, "#!/bin/sh\necho first-runnable-bundle\n")?;
        fs::set_permissions(&marker_script, fs::Permissions::from_mode(0o755))?;
        let candidate = begin_runtime_layout(project.path(), &resolved)?;
        materialize_official_store(
            candidate.path(),
            &resolved,
            true,
            Some(complete_officials.path()),
            &MaterializeSettings::default(),
            |_, _| unreachable!("no path-overridden official in this fixture"),
        )?;
        let staged_root = publish_runtime_layout(candidate, &resolved)?;
        let published_binary = staged_root.join("bin").join(&binary_name);
        assert_eq!(run_marker(&published_binary)?, "first-runnable-bundle\n");

        // Second refresh: `officials_source` is present (as a container run
        // would be) but incomplete - `bin/` is missing `mission` entirely.
        // `materialize_official_store` must hard error rather than silently
        // falling back to a host-side `cargo install`.
        let incomplete_officials = tempfile::tempdir()?;
        fs::create_dir_all(incomplete_officials.path().join("bin"))?;
        let next_candidate = begin_runtime_layout(project.path(), &resolved)?;
        let candidate_dir = next_candidate.path().to_path_buf();
        let error = materialize_official_store(
            next_candidate.path(),
            &resolved,
            true,
            Some(incomplete_officials.path()),
            &MaterializeSettings::default(),
            |_, _| unreachable!("no path-overridden official in this fixture"),
        )
        .expect_err("an officials_source missing an expected official must hard error");
        assert!(
            error.to_string().contains("mission"),
            "error should name the missing official: {error}"
        );

        // The caller never publishes on this error path (the same `?`
        // early-return every real caller uses) - dropping the candidate here
        // reproduces that, and its `TempDir` cleans itself up.
        drop(next_candidate);
        assert!(
            !candidate_dir.exists(),
            "a discarded candidate must not leak its temp directory"
        );

        // The previously published bundle is completely untouched: same
        // path, same bytes, and genuinely still executes successfully.
        assert!(staged_root.is_dir());
        assert_eq!(run_marker(&published_binary)?, "first-runnable-bundle\n");
        Ok(())
    }

    /// Execute the marker script at `path` and return its captured stdout,
    /// failing loudly if it did not exit successfully - the literal proof
    /// half of [`materialization_failure_leaves_previous_bundle_intact_and_runnable`].
    fn run_marker(path: &Path) -> Result<String> {
        let output = std::process::Command::new(path)
            .output()
            .with_context(|| format!("failed to execute marker binary {}", path.display()))?;
        ensure!(
            output.status.success(),
            "marker binary {} exited with {}: {}",
            path.display(),
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout)
            .with_context(|| format!("marker binary {} wrote non-UTF8 stdout", path.display()))
    }

    #[test]
    fn stages_a_participant_binary_into_a_flat_bin_store_by_hardlink_identity() -> Result<()> {
        let _scratch = ScratchPhoxalHome::new()?;
        let project = tempfile::tempdir()?;
        fs::create_dir_all(project.path().join("model"))?;
        fs::write(project.path().join("model/structure.urdf"), "<robot/>")?;
        let resolved = resolved_robot()?;
        let staged = stage_runtime_layout(project.path(), &resolved)?;

        // A "built workspace artifact" living in a cargo-style target dir.
        let target_dir = project.path().join("target/debug");
        fs::create_dir_all(&target_dir)?;
        let built = target_dir.join("mission");
        fs::write(&built, "ELF")?;

        let record = launch_record(
            "mission",
            "mission",
            ParticipantExecution::UserService {
                binary_name: "mission".to_string(),
            },
            None,
        );
        let staged_bin = stage_participant_binary(&staged, &record, &built)?;

        // The returned path is the flat store entry, not the cargo target path.
        assert_eq!(staged_bin, staged.join("bin/mission"));
        assert!(staged_bin.is_file());
        // Hardlink identity: same inode as the built artifact.
        assert_eq!(
            fs::metadata(&built)?.ino(),
            fs::metadata(&staged_bin)?.ino(),
            "bin/ entry must be a hardlink to the built artifact"
        );

        // A refresh after the artifact changes relinks - bin/ never goes stale.
        fs::remove_file(&built)?;
        fs::write(&built, "ELF-v2")?;
        let refreshed = stage_participant_binary(&staged, &record, &built)?;
        assert_eq!(fs::read_to_string(&refreshed)?, "ELF-v2");
        assert_eq!(fs::metadata(&built)?.ino(), fs::metadata(&refreshed)?.ino());
        Ok(())
    }

    #[test]
    fn copy_fallback_reproduces_bytes_and_mode_with_a_distinct_inode() -> Result<()> {
        // The cross-filesystem fallback path `link_or_copy` takes when
        // `hard_link` fails: identical bytes and executable bits, distinct inode.
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir()?;
        let source = dir.path().join("src");
        fs::write(&source, "ELF")?;
        fs::set_permissions(&source, fs::Permissions::from_mode(0o755))?;
        let dest = dir.path().join("bin/dest");
        fs::create_dir_all(dest.parent().unwrap())?;

        copy_binary(&source, &dest)?;
        assert_eq!(fs::read_to_string(&dest)?, "ELF");
        assert_ne!(fs::metadata(&source)?.ino(), fs::metadata(&dest)?.ino());
        assert_eq!(fs::metadata(&dest)?.permissions().mode() & 0o111, 0o111);
        Ok(())
    }

    /// Every participant, on every host - the darwin path included - gets a flat
    /// `bin/` entry; there is no `.app`-bundle carve-out (#936, finding D). A
    /// robot participant is always a plain executable (a cargo-built binary),
    /// so even a tool whose source lives under a `.app`-shaped path is
    /// flattened into `bin/` under its canonical identity, and the loader
    /// resolves it there with no host-specific tolerance.
    #[test]
    fn a_participant_is_always_flattened_into_bin_even_on_darwin() -> Result<()> {
        let _scratch = ScratchPhoxalHome::new()?;
        let project = tempfile::tempdir()?;
        fs::create_dir_all(project.path().join("model"))?;
        fs::write(project.path().join("model/structure.urdf"), "<robot/>")?;
        let resolved = resolved_robot()?;
        let staged = stage_runtime_layout(project.path(), &resolved)?;

        // A plain source executable, as a cargo-built binary always is -
        // never a `.app` bundle.
        let source_dir = project.path().join("target/debug");
        fs::create_dir_all(&source_dir)?;
        let source = source_dir.join("phoxal-tool-joypad");
        fs::write(&source, "MACHO")?;

        let record = launch_record(
            "tool-joypad-testbot",
            "tool-joypad",
            ParticipantExecution::OfficialTool {
                binary_name: "phoxal-tool-joypad".to_string(),
            },
            None,
        );
        let executable = stage_participant_binary(&staged, &record, &source)?;
        // The flat `bin/` entry is created and returned - strict everywhere.
        assert_eq!(executable, staged.join("bin/phoxal-tool-joypad"));
        assert!(executable.is_file());
        assert_eq!(fs::read_to_string(&executable)?, "MACHO");
        Ok(())
    }

    #[test]
    fn canonical_binary_names_are_identity_keyed_across_sources() {
        // The source-free plan (#936) names each participant's `bin/` binary on
        // its execution, so `canonical_binary_name` is that name and the loader
        // resolves the identical one from the compiled `robot.yaml`.
        assert_eq!(
            canonical_binary_name(&launch_record(
                "drive",
                "drive",
                ParticipantExecution::OfficialArtifact {
                    binary_name: "phoxal-service-drive".to_string()
                },
                None,
            )),
            "phoxal-service-drive"
        );
        // A user service is stored under its own identity.
        assert_eq!(
            canonical_binary_name(&launch_record(
                "mission",
                "mission",
                ParticipantExecution::UserService {
                    binary_name: "mission".to_string(),
                },
                None,
            )),
            "mission"
        );
        // An official tool: the kind-prefixed short name.
        assert_eq!(
            canonical_binary_name(&launch_record(
                "tool-bus-testbot",
                "tool-bus",
                ParticipantExecution::OfficialTool {
                    binary_name: "phoxal-tool-bus".to_string()
                },
                None,
            )),
            "phoxal-tool-bus"
        );
        // A component driver is named by its component id and shared across
        // every instance - whether built from source or resolved from the
        // registry.
        for instance in ["left_drive", "right_drive"] {
            assert_eq!(
                canonical_binary_name(&launch_record(
                    instance,
                    "ddsm115",
                    ParticipantExecution::ComponentDriver {
                        binary_name: "phoxal-component-ddsm115".to_string(),
                    },
                    Some(instance),
                )),
                "phoxal-component-ddsm115"
            );
        }
    }

    #[test]
    fn stages_officials_and_shared_drivers_under_canonical_identity_names() -> Result<()> {
        let _scratch = ScratchPhoxalHome::new()?;
        let project = tempfile::tempdir()?;
        fs::create_dir_all(project.path().join("model"))?;
        fs::write(project.path().join("model/structure.urdf"), "<robot/>")?;
        let resolved = resolved_robot()?;
        let staged = stage_runtime_layout(project.path(), &resolved)?;

        let target_dir = project.path().join("target/debug");
        fs::create_dir_all(&target_dir)?;
        // A cargo-built official-service override and one driver binary shared
        // by two component instances - both under their local cargo names.
        let drive = target_dir.join("robot-service-drive");
        fs::write(&drive, "DRIVE")?;
        let driver = target_dir.join("ddsm115-driver");
        fs::write(&driver, "DDSM")?;

        // The source-overridden official lands at the canonical name.
        let staged_drive = stage_participant_binary(
            &staged,
            &launch_record(
                "drive",
                "drive",
                ParticipantExecution::OfficialArtifact {
                    binary_name: "phoxal-service-drive".to_string(),
                },
                None,
            ),
            &drive,
        )?;
        assert_eq!(staged_drive, staged.join("bin/phoxal-service-drive"));
        assert!(staged_drive.is_file());

        // One driver binary, one bin/ entry, both instances resolve to it.
        let left = stage_participant_binary(
            &staged,
            &launch_record(
                "left_drive",
                "ddsm115",
                ParticipantExecution::ComponentDriver {
                    binary_name: "phoxal-component-ddsm115".to_string(),
                },
                Some("left_drive"),
            ),
            &driver,
        )?;
        let right = stage_participant_binary(
            &staged,
            &launch_record(
                "right_drive",
                "ddsm115",
                ParticipantExecution::ComponentDriver {
                    binary_name: "phoxal-component-ddsm115".to_string(),
                },
                Some("right_drive"),
            ),
            &driver,
        )?;
        let shared = staged.join("bin/phoxal-component-ddsm115");
        assert_eq!(left, shared);
        assert_eq!(right, shared);
        assert!(shared.is_file());
        assert_eq!(fs::metadata(&driver)?.ino(), fs::metadata(&shared)?.ino());
        Ok(())
    }

    #[test]
    fn stages_component_bundle_without_mutating_its_source_tree() -> Result<()> {
        let _scratch = ScratchPhoxalHome::new()?;
        let project = tempfile::tempdir()?;
        fs::create_dir_all(project.path().join("model"))?;
        fs::write(project.path().join("model/structure.urdf"), "robot")?;
        let source = project.path().join("components/ddsm115");
        fs::create_dir_all(source.join("meshes"))?;
        fs::write(source.join("component.yaml"), "schema: component/v0\n")?;
        fs::write(source.join("simulation.yaml"), "device: motor\n")?;
        fs::write(source.join("meshes/wheel.dae"), "wheel")?;
        let source_before = [
            fs::read(source.join("component.yaml"))?,
            fs::read(source.join("simulation.yaml"))?,
            fs::read(source.join("meshes/wheel.dae"))?,
        ];

        let mut resolved = resolved_robot()?;
        resolved.components.push(ResolvedComponent {
            instance: "left_drive".to_string(),
            source_name: "ddsm115".to_string(),
            assets: (ResolvedComponentPackage {
                package: "phoxal/component-ddsm115".to_string(),
                kind: ArtifactKind::ComponentAssets,
                source: ResolvedComponentSource::Path {
                    path: PathBuf::from("components/ddsm115"),
                },
                resolved_dir: Some(source.clone()),
                registry_runtime: None,
            }),
            driver: None,
            has_driver: false,
        });

        let staged = stage_runtime_layout(project.path(), &resolved)?;

        let component = staged.join("components/ddsm115");
        assert!(component.join("component.yaml").is_file());
        assert!(component.join("simulation.yaml").is_file());
        assert!(component.join("meshes/wheel.dae").is_file());
        assert_eq!(
            source_before,
            [
                fs::read(source.join("component.yaml"))?,
                fs::read(source.join("simulation.yaml"))?,
                fs::read(source.join("meshes/wheel.dae"))?,
            ],
            "staging must leave source component assets unchanged"
        );
        Ok(())
    }

    fn platform_runtime(name: &str, kind: ArtifactKind) -> ResolvedPlatformRuntime {
        let dir = match kind {
            ArtifactKind::Service => "service",
            ArtifactKind::Simulator => "simulator",
            _ => unreachable!("only services/simulators are platform runtimes here"),
        };
        ResolvedPlatformRuntime {
            name: name.to_string(),
            package: format!("phoxal/{dir}-{name}"),
            kind,
            path_override: None,
            train: "0.36.0".to_string(),
            target: Some(host_target_triple()),
        }
    }

    fn router_tool() -> ResolvedTool {
        official_tool("infrastructure-router", ArtifactKind::Infrastructure)
    }

    fn official_tool(short: &str, kind: ArtifactKind) -> ResolvedTool {
        let (dir, prefix) = match kind {
            ArtifactKind::Tool => ("tool", "tool-"),
            ArtifactKind::Infrastructure => ("infrastructure", "infrastructure-"),
            _ => unreachable!("only tools/infrastructure resolve as tools here"),
        };
        let artifact = short.strip_prefix(prefix).unwrap_or(short);
        ResolvedTool {
            kind,
            name: short.to_string(),
            package: format!("phoxal/{dir}-{artifact}"),
            binary_name: official_binary_name(kind, artifact),
            path_override: None,
            train: "0.36.0".to_string(),
            target: host_target_triple(),
        }
    }

    /// A source-overridden dormant official is built through `build_override`
    /// and never reaches `cargo install` - this is the one materialization
    /// path unit-testable with no network. The `cargo install` path itself is
    /// covered by `crate::materialize`'s own command-construction tests.
    #[test]
    fn materialize_builds_source_overridden_dormant_officials() -> Result<()> {
        let _scratch = ScratchPhoxalHome::new()?;
        let project = tempfile::tempdir()?;
        fs::create_dir_all(project.path().join("model"))?;
        fs::write(project.path().join("model/structure.urdf"), "<robot/>")?;
        let mut resolved = resolved_robot()?;
        let mut drive = platform_runtime("drive", ArtifactKind::Service);
        drive.path_override = Some(PathBuf::from("services/drive"));
        resolved.platform_runtimes = vec![drive];
        let staged = stage_runtime_layout(project.path(), &resolved)?;

        // The override "build" produces a real artifact the store links from.
        let built = project.path().join("target/debug/robot-service-drive");
        fs::create_dir_all(built.parent().unwrap())?;
        fs::write(&built, "OVERRIDE")?;
        materialize_official_store(
            &staged,
            &resolved,
            false,
            None,
            &MaterializeSettings::default(),
            |crate_dir, name| {
                assert_eq!(crate_dir, Path::new("services/drive"));
                assert_eq!(name, "drive");
                Ok(built.clone())
            },
        )?;

        assert_eq!(
            fs::read_to_string(staged.join("bin/phoxal-service-drive"))?,
            "OVERRIDE"
        );
        Ok(())
    }

    #[test]
    fn stage_router_binary_links_only_the_router_when_source_overridden() -> Result<()> {
        let _scratch = ScratchPhoxalHome::new()?;
        let project = tempfile::tempdir()?;
        fs::create_dir_all(project.path().join("model"))?;
        fs::write(project.path().join("model/structure.urdf"), "<robot/>")?;
        let mut resolved = resolved_robot()?;
        resolved.platform_runtimes = vec![platform_runtime("drive", ArtifactKind::Service)];
        let mut router = router_tool();
        router.path_override = Some(PathBuf::from("tools/infrastructure-router"));
        resolved.tools = vec![router];
        let staged = stage_runtime_layout(project.path(), &resolved)?;

        let built = project.path().join("target/debug/router");
        fs::create_dir_all(built.parent().unwrap())?;
        fs::write(&built, "ROUTER")?;
        stage_router_binary(
            &staged,
            &resolved,
            false,
            &MaterializeSettings::default(),
            |_, _| Ok(built.clone()),
        )?;

        assert_eq!(
            fs::read_to_string(staged.join("bin/phoxal-infrastructure-router"))?,
            "ROUTER"
        );
        // The router-only step never stages dormant services.
        assert!(!staged.join("bin/phoxal-service-drive").exists());
        Ok(())
    }
}
