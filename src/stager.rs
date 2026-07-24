//! The unified runtime-layout stager.
//!
//! One stager materializes a source project into the runtime layout at
//! `.phoxal/build/<triple>/` (host triple for `run` and live simulation):
//!
//! ```text
//! robot.yaml    # fully flattened, resolved robot/v0 with the complete service map
//! bin/          # flat binary lookup store (hardlinks/copies, refreshed every pass)
//! model/        # structure/URDF assets, when referenced
//! components/   # compiled runtime component assets
//! behaviors/    # when referenced
//! ```
//!
//! The compiled `robot.yaml` + assets swap is atomic per refresh (stage into a
//! sibling temp dir, then rename), so a crashed pass never leaves a
//! half-written layout. `bin/` is (re)linked from the resolved binaries every
//! refresh so it can never go stale. The staged layout contains no Cargo
//! manifests, no source, and no `.phoxal` of its own; runtime state
//! (project.lock, supervisor.sock, plans) stays in the project root's
//! `.phoxal/`, never inside the staged layout.
//!
//! This replaces the previous source staging that published a single
//! `.phoxal/run/robot` directory; `run` and live simulation now share this one
//! stager. Building the flat `bin/` from an extracted bundle with no source
//! present - the universal staged-root loader - is the next slice; `run` still
//! requires a source project root today.

use std::fs;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, ensure};

use phoxal_cli_core::artifacts::NativeArtifactDescriptor;
use phoxal_cli_core::project::launch_plan::{ParticipantLaunchRecord, runtime_layout_dir};
use phoxal_cli_core::project::resolver::{
    ResolvedPlatformRuntime, ResolvedRobot, ResolvedTool, official_binary_name, tool_emit_apis_id,
};
use phoxal_cli_core::project::suite::ArtifactKind;

const PREVIOUS_LAYOUT_SUFFIX: &str = ".previous";
const BEHAVIORS_DIR: &str = "behaviors";
const MESHES_DIR: &str = "meshes";
const BIN_DIR: &str = "bin";

/// The staged runtime layout directory for this resolved robot's target under
/// `project_root`. `run` and live simulation stage and execute the host triple.
#[must_use]
pub fn layout_path(project_root: &Path, resolved: &ResolvedRobot) -> PathBuf {
    runtime_layout_dir(project_root, &resolved.target)
}

/// Stage the compiled `robot.yaml` and runtime assets into
/// `.phoxal/build/<triple>/`, atomically replacing any previous layout. The
/// caller owns the project run lock for the whole operation, so no participant
/// observes the brief exchange between the previous complete layout and the
/// newly validated candidate. `bin/` is created empty; callers that execute the
/// layout populate it with [`stage_participant_binary`] and
/// [`stage_complete_official_store`].
pub fn stage_runtime_layout(project_root: &Path, resolved: &ResolvedRobot) -> Result<PathBuf> {
    let build_dir =
        project_root.join(phoxal_cli_core::project::launch_plan::RUNTIME_BUILD_ROOT_RELATIVE);
    fs::create_dir_all(&build_dir).with_context(|| {
        format!(
            "failed to create runtime layout directory {}",
            build_dir.display()
        )
    })?;
    let candidate = tempfile::Builder::new()
        .prefix(&format!(".{}-candidate-", resolved.target))
        .tempdir_in(&build_dir)
        .with_context(|| {
            format!(
                "failed to create runtime layout candidate in {}",
                build_dir.display()
            )
        })?;

    let compiled = compile_manifest(resolved);
    stage_candidate(project_root, candidate.path(), resolved, &compiled)?;
    validate_candidate(candidate.path(), resolved, &compiled)?;

    let target = layout_path(project_root, resolved);
    let previous = build_dir.join(format!(".{}{PREVIOUS_LAYOUT_SUFFIX}", resolved.target));
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

/// The compiled `robot/v0` manifest for the staged layout: the resolved robot
/// with a *complete* service map. The compiled runtime has no Cargo graph to
/// discover from, so every discovered user service (`services/<name>`) is
/// enumerated even when the authored `robot.yaml` omitted it, carrying its
/// final validated config (`None` when the author declared none). The `extends:`
/// chain was already flattened by the framework loader
/// (`Robot::read_from_path`), so the map already holds the resolved authored
/// entries; this only fills in the discovery-only services.
fn compile_manifest(resolved: &ResolvedRobot) -> phoxal::model::robot::v0::Robot {
    let mut compiled = resolved.robot.clone();
    for runtime in &resolved.user_runtimes {
        compiled
            .services
            .entry(runtime.name.clone())
            .or_insert_with(|| phoxal::model::robot::v0::UserService { config: None });
    }
    compiled
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
/// participant runs from. `source` is the built or vendored binary the plan
/// participant resolves to; `bin/` is a flat identity-keyed store, so one driver
/// binary shared by several component instances is linked once under its
/// component id, and a source-overridden official lands at the same name a
/// vendored one would - the loader resolves both identically.
///
/// No symlinks and no `.app` bundles: the staged layout holds real file
/// identities that keep working if `target/` is later cleaned. Every
/// participant, on every host, gets a flat `bin/` entry - a robot participant is
/// always a plain executable (a cargo-built `target/` binary or a vendored flat
/// artifact), never a macOS `.app` bundle. The only `.app` in the whole system
/// is the Webots *application* on the `simulate` path, which is a CLI-managed
/// host process outside the plan/layout contract entirely (its bundle is handled
/// by the supervisor's `materialize_macos_app_binary`, never here).
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
/// official runtime it is resolved through the layout's flat `bin/` store, not
/// from the vendored artifact store directly.
#[must_use]
pub fn staged_router_binary(staged_root: &Path) -> PathBuf {
    staged_root
        .join(BIN_DIR)
        .join(official_binary_name(ArtifactKind::Infrastructure, "router"))
}

/// Complete the staged `bin/` into the loader's full required lookup store.
///
/// [`stage_participant_binary`] only stages the officials that appear as active
/// plan participants, but the loader
/// ([`required_runtimes`](phoxal_cli_core::project::layout::RuntimeLayout::required_runtimes))
/// requires *every* catalog official plus the infrastructure router: per #945
/// every official always runs, an official with no active workload simply stays
/// dormant. This links the remainder of that required native set - every
/// dormant catalog service and the router - into `bin/` under the same
/// canonical identity names the loader resolves against, so `bin/` is the true
/// complete store an extracted bundle can be executed from with no source.
///
/// Entries already present (a referenced official, or a source override staged
/// by [`stage_participant_binary`]) are left in place. A source-overridden dormant
/// official is built through `build_override`; every other missing official is
/// linked from the vendored `.phoxal/artifacts` store, and a store miss fails
/// with the "run `phoxal update`" error without ever touching the network.
pub fn stage_complete_official_store(
    staged_root: &Path,
    resolved: &ResolvedRobot,
    mut build_override: impl FnMut(&Path, &str) -> Result<PathBuf>,
) -> Result<()> {
    let bin_dir = ensure_bin_dir(staged_root)?;
    for runtime in &resolved.platform_runtimes {
        ensure_platform_staged(&bin_dir, runtime, &mut build_override)?;
    }
    for tool in &resolved.tools {
        ensure_tool_staged(&bin_dir, tool, &mut build_override)?;
    }
    Ok(())
}

/// Stage only the infrastructure router into `bin/`, so it launches from the
/// staged store like every other official. Used by the Webots path, whose
/// remaining runtime set is staged through its own (simulation-managed) route
/// (#931); the router itself is CLI-supervised identically to a native run.
pub fn stage_router_binary(
    staged_root: &Path,
    resolved: &ResolvedRobot,
    mut build_override: impl FnMut(&Path, &str) -> Result<PathBuf>,
) -> Result<()> {
    let bin_dir = ensure_bin_dir(staged_root)?;
    let router = resolved
        .tools
        .iter()
        .find(|tool| tool.kind == ArtifactKind::Infrastructure)
        .context("resolved graph is missing the infrastructure router; run `phoxal update`")?;
    ensure_tool_staged(&bin_dir, router, &mut build_override)
}

fn ensure_bin_dir(staged_root: &Path) -> Result<PathBuf> {
    let bin_dir = staged_root.join(BIN_DIR);
    fs::create_dir_all(&bin_dir)
        .with_context(|| format!("failed to create staged bin store {}", bin_dir.display()))?;
    Ok(bin_dir)
}

/// Ensure one official platform runtime (a service or simulator) has a canonical
/// flat `bin/` entry, resolving it from a source override or the vendored store.
fn ensure_platform_staged(
    bin_dir: &Path,
    runtime: &ResolvedPlatformRuntime,
    build_override: &mut impl FnMut(&Path, &str) -> Result<PathBuf>,
) -> Result<()> {
    let staged = bin_dir.join(official_binary_name(runtime.kind, &runtime.name));
    if staged.is_file() {
        return Ok(());
    }
    let source = resolve_platform_source(runtime, build_override)?;
    link_official_source(&source, &staged)
}

/// Ensure one official tool (or the infrastructure router) has a canonical
/// `bin/` entry, resolving it from a source override or the vendored store.
fn ensure_tool_staged(
    bin_dir: &Path,
    tool: &ResolvedTool,
    build_override: &mut impl FnMut(&Path, &str) -> Result<PathBuf>,
) -> Result<()> {
    let staged = bin_dir.join(&tool.binary_name);
    if staged.is_file() {
        return Ok(());
    }
    let source = resolve_tool_source(tool, build_override)?;
    link_official_source(&source, &staged)
}

/// Resolve the source binary for one official platform runtime (a service,
/// simulator, or suite-sourced component driver): its source override built
/// through `build_override`, or the vendored artifact from `.phoxal/artifacts`.
/// Hard-fails on a store miss ("run `phoxal update`") and never touches the
/// network - the loader-driven execution path resolves every official binary
/// this way, no env-var fallbacks.
pub fn resolve_platform_source(
    runtime: &ResolvedPlatformRuntime,
    build_override: &mut impl FnMut(&Path, &str) -> Result<PathBuf>,
) -> Result<PathBuf> {
    if let Some(crate_dir) = &runtime.path_override {
        return build_override(crate_dir, &runtime.name);
    }
    let descriptor = NativeArtifactDescriptor::from_runtime(runtime)?.with_context(|| {
        format!(
            "official runtime {} has no vendored artifact to stage; run `phoxal update`",
            runtime.package
        )
    })?;
    resolve_vendored_binary(&descriptor)
}

/// Resolve the source binary for one official tool (or the infrastructure
/// router), the tool counterpart of [`resolve_platform_source`].
pub fn resolve_tool_source(
    tool: &ResolvedTool,
    build_override: &mut impl FnMut(&Path, &str) -> Result<PathBuf>,
) -> Result<PathBuf> {
    if let Some(crate_dir) = &tool.path_override {
        return build_override(crate_dir, tool_emit_apis_id(&tool.name));
    }
    let descriptor = NativeArtifactDescriptor::from_tool(tool)?.with_context(|| {
        format!(
            "official runtime {} has no vendored artifact to stage; run `phoxal update`",
            tool.package
        )
    })?;
    resolve_vendored_binary(&descriptor)
}

/// Hardlink a resolved official binary into its flat `bin/` entry. Every
/// official, on every host, is a plain executable and is flattened - there is no
/// `.app`-bundle carve-out (see [`stage_participant_binary`]).
fn link_official_source(source: &Path, staged: &Path) -> Result<()> {
    link_or_copy(source, staged)
}

/// Resolve a vendored official/tool binary from the project-local
/// `.phoxal/artifacts` store, failing with a "run `phoxal update`" error and
/// never touching the network when the store lacks it. Staging links from the
/// vendored store only; it never fetches.
pub(crate) fn resolve_vendored_binary(descriptor: &NativeArtifactDescriptor) -> Result<PathBuf> {
    let binary = crate::native_artifacts::artifact_binary_path(descriptor).with_context(|| {
        format!(
            "vendored artifact {} is not in the project artifact store; run `phoxal update`",
            descriptor.binary_name
        )
    })?;
    ensure!(
        binary.is_file(),
        "vendored artifact {} is not in the project artifact store ({}); run `phoxal update`",
        descriptor.binary_name,
        binary.display()
    );
    Ok(binary)
}

/// Hardlink `source` to `dest`, falling back to a byte copy when hardlinking
/// fails (e.g. `source` and `dest` are on different filesystems). Any existing
/// `dest` is removed first so a refresh always relinks to the current bytes.
fn link_or_copy(source: &Path, dest: &Path) -> Result<()> {
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

    crate::native_artifacts::stage_component_bundles_into_robot_root(
        project_root,
        candidate,
        resolved,
    )
    .context("failed to stage component assets into the runtime layout")
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
    // Resolution already ran the model's semantic validation against the
    // suite's platform names. Reparse the serialized candidate here to prove
    // the on-disk manifest is complete and strict without losing that
    // owner-specific validation context.
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
    use phoxal_cli_core::project::launch_plan::ParticipantExecution;
    use phoxal_cli_core::project::resolver::{
        ResolvedComponent, ResolvedComponentPackage, ResolvedComponentSource, ResolvedRobot,
        ResolvedUserRuntime,
    };
    use phoxal_cli_core::project::suite::ArtifactKind;
    use std::os::unix::fs::MetadataExt;

    fn resolved_robot() -> Result<ResolvedRobot> {
        let yaml = r#"schema: robot/v0
robot:
  id: robot_v1
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
                incarnation: 0,
                namespace: "dev".to_string(),
                robot_id: "robot_v1".to_string(),
                bus: BusProfile {
                    connect_endpoints: Vec::new(),
                },
                clock: ClockMode::Real,
                config: None,
                robot_root: None,
                component_instance: component_instance.map(str::to_string),
                execution_device_id: None,
                shutdown_grace_ms: DEFAULT_SHUTDOWN_GRACE_MS,
            },
            launch_ownership: Default::default(),
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
        assert!(staged.starts_with(project.path().join(".phoxal/build")));

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
        assert!(
            !project
                .path()
                .join(format!(".phoxal/build/.{}.previous", resolved.target))
                .exists()
        );
        Ok(())
    }

    #[test]
    fn compiled_manifest_enumerates_services_the_authored_yaml_omitted() -> Result<()> {
        let _scratch = ScratchPhoxalHome::new()?;
        let project = tempfile::tempdir()?;
        fs::create_dir_all(project.path().join("model"))?;
        fs::write(project.path().join("model/structure.urdf"), "<robot/>")?;

        let mut resolved = resolved_robot()?;
        // Authored config for one service; a second service discovered from the
        // Cargo graph with no authored entry.
        resolved.robot.services.insert(
            "mission".to_string(),
            phoxal::model::robot::v0::UserService {
                config: Some(serde_json::json!({"speed": 1})),
            },
        );
        resolved.user_runtimes.push(ResolvedUserRuntime {
            name: "mission".to_string(),
            path: PathBuf::from("services/mission"),
            source_hash: "hash".to_string(),
        });
        resolved.user_runtimes.push(ResolvedUserRuntime {
            name: "telemetry".to_string(),
            path: PathBuf::from("services/telemetry"),
            source_hash: "hash".to_string(),
        });

        let staged = stage_runtime_layout(project.path(), &resolved)?;
        let compiled = phoxal::model::robot::Robot::parse_from_dir(&staged)?
            .as_v0()
            .clone();
        // Every discovered user service is present, authored config preserved,
        // the discovery-only service defaulted to no config.
        assert_eq!(
            compiled.services.keys().collect::<Vec<_>>(),
            vec!["mission", "telemetry"]
        );
        assert_eq!(
            compiled.services["mission"].config,
            Some(serde_json::json!({"speed": 1}))
        );
        assert_eq!(compiled.services["telemetry"].config, None);
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

            let build_dir = project.path().join(".phoxal/build");
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

    #[test]
    fn resolve_vendored_binary_errors_with_update_hint_without_network() -> Result<()> {
        let _scratch = ScratchPhoxalHome::new()?;
        let descriptor = NativeArtifactDescriptor {
            package_id: "phoxal/service-drive".to_string(),
            kind: ArtifactKind::Service,
            name: "drive".to_string(),
            version: "0.36.0".to_string(),
            url: String::new(),
            sha256: String::new(),
            size: 0,
            binary_name: "phoxal-service-drive".to_string(),
            target: Some(host_target_triple()),
        };
        let error = resolve_vendored_binary(&descriptor)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("phoxal update"),
            "missing vendored artifact must point at `phoxal update`: {error}"
        );
        Ok(())
    }

    /// Every participant, on every host - the darwin path included - gets a flat
    /// `bin/` entry; there is no `.app`-bundle carve-out (#936, finding D). A
    /// robot participant is always a plain executable (a cargo-built binary or a
    /// vendored flat artifact - vendored darwin artifacts are flat files too), so
    /// even a tool whose source lives under a `.app`-shaped path is flattened
    /// into `bin/` under its canonical identity, and the loader resolves it
    /// there with no host-specific tolerance.
    #[test]
    fn a_participant_is_always_flattened_into_bin_even_on_darwin() -> Result<()> {
        let _scratch = ScratchPhoxalHome::new()?;
        let project = tempfile::tempdir()?;
        fs::create_dir_all(project.path().join("model"))?;
        fs::write(project.path().join("model/structure.urdf"), "<robot/>")?;
        let resolved = resolved_robot()?;
        let staged = stage_runtime_layout(project.path(), &resolved)?;

        // A plain source executable, as a vendored darwin tool artifact or a
        // cargo-built binary always is - never a `.app` bundle.
        let source_dir = project.path().join("target/debug");
        fs::create_dir_all(&source_dir)?;
        let source = source_dir.join("phoxal-tool-joypad");
        fs::write(&source, "MACHO")?;

        let record = launch_record(
            "tool-joypad-robot_v1",
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
                "tool-bus-robot_v1",
                "tool-bus",
                ParticipantExecution::OfficialTool {
                    binary_name: "phoxal-tool-bus".to_string()
                },
                None,
            )),
            "phoxal-tool-bus"
        );
        // A component driver is named by its component id and shared across
        // every instance - whether built from source or resolved from the suite.
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

        // The source-overridden official lands at the vendored canonical name.
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
                path_override: None,
                suite_runtime: None,
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
            version: "0.36.0".to_string(),
            artifact_ref: "ref".to_string(),
            sha256: None,
            url: None,
            size: None,
            published: true,
            published_triples: vec![host_target_triple()],
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
            requested: "0.36.0".to_string(),
            resolved: "0.36.0".to_string(),
            repo: "vendored".to_string(),
            asset: "ref".to_string(),
            binary_name: official_binary_name(kind, artifact),
            sha256: String::new(),
            url: None,
            size: None,
            published: true,
            path_override: None,
            train: "0.36.0".to_string(),
            target: host_target_triple(),
        }
    }

    /// Place a plain (non-`.app`) binary in the project-vendored artifact store
    /// under the descriptor's active version, so `resolve_vendored_binary`
    /// resolves it exactly as a `phoxal update` would have vendored it.
    fn seed_vendored(descriptor: &NativeArtifactDescriptor, bytes: &[u8]) -> Result<()> {
        let dir = crate::native_artifacts::artifact_exec_dir(descriptor)?;
        fs::create_dir_all(&dir)?;
        fs::write(dir.join(&descriptor.binary_name), bytes)?;
        crate::native_artifacts::retarget_active(descriptor)?;
        Ok(())
    }

    #[test]
    fn complete_store_links_dormant_officials_and_router_and_keeps_participants() -> Result<()> {
        let _scratch = ScratchPhoxalHome::new()?;
        let project = tempfile::tempdir()?;
        fs::create_dir_all(project.path().join("model"))?;
        fs::write(project.path().join("model/structure.urdf"), "<robot/>")?;
        let mut resolved = resolved_robot()?;
        // `drive` is an active participant already linked into bin/; `motion` is
        // a dormant official; the router and `tool-bus` are officials that never
        // appear as plan participants.
        resolved.platform_runtimes = vec![
            platform_runtime("drive", ArtifactKind::Service),
            platform_runtime("motion", ArtifactKind::Service),
        ];
        resolved.tools = vec![router_tool(), official_tool("tool-bus", ArtifactKind::Tool)];
        let staged = stage_runtime_layout(project.path(), &resolved)?;

        // The active participant is already present with distinctive bytes.
        fs::create_dir_all(staged.join(BIN_DIR))?;
        fs::write(staged.join("bin/phoxal-service-drive"), b"PARTICIPANT")?;

        // Vendor the officials that are not already linked.
        seed_vendored(
            &NativeArtifactDescriptor::from_runtime(&resolved.platform_runtimes[1])?.unwrap(),
            b"MOTION",
        )?;
        seed_vendored(
            &NativeArtifactDescriptor::from_tool(&resolved.tools[0])?.unwrap(),
            b"ROUTER",
        )?;
        seed_vendored(
            &NativeArtifactDescriptor::from_tool(&resolved.tools[1])?.unwrap(),
            b"BUS",
        )?;

        stage_complete_official_store(&staged, &resolved, |_, name| {
            panic!("no source override expected for {name}")
        })?;

        // The pre-linked participant is left untouched.
        assert_eq!(
            fs::read_to_string(staged.join("bin/phoxal-service-drive"))?,
            "PARTICIPANT"
        );
        // Every dormant official and the router are now present under their
        // canonical identity names.
        assert_eq!(
            fs::read_to_string(staged.join("bin/phoxal-service-motion"))?,
            "MOTION"
        );
        assert_eq!(
            fs::read_to_string(staged.join("bin/phoxal-infrastructure-router"))?,
            "ROUTER"
        );
        assert_eq!(
            fs::read_to_string(staged.join("bin/phoxal-tool-bus"))?,
            "BUS"
        );
        assert_eq!(
            staged_router_binary(&staged),
            staged.join("bin/phoxal-infrastructure-router")
        );
        Ok(())
    }

    /// `phoxal build --target <TRIPLE>` stages into `.phoxal/build/<triple>/` and
    /// resolves the officials from the suite's per-target vendored blobs; when
    /// those blobs are not vendored it fails with the "run `phoxal update`"
    /// error, without ever touching the network - exactly like a host build.
    #[test]
    fn cross_target_official_store_points_at_phoxal_update_without_network() -> Result<()> {
        let _scratch = ScratchPhoxalHome::new()?;
        let project = tempfile::tempdir()?;
        fs::create_dir_all(project.path().join("model"))?;
        fs::write(project.path().join("model/structure.urdf"), "<robot/>")?;
        let foreign = "aarch64-unknown-linux-gnu";
        let mut resolved = resolved_robot()?;
        resolved.target = foreign.to_string();
        let mut drive = platform_runtime("drive", ArtifactKind::Service);
        drive.target = Some(foreign.to_string());
        resolved.platform_runtimes = vec![drive];

        // Staging lands under the requested target triple, not the host.
        let staged = stage_runtime_layout(project.path(), &resolved)?;
        assert!(
            staged.starts_with(project.path().join(".phoxal/build").join(foreign)),
            "cross-target staging must land under .phoxal/build/{foreign}/: {}",
            staged.display()
        );

        let error = stage_complete_official_store(&staged, &resolved, |_, name| {
            panic!("no source override expected for {name}")
        })
        .unwrap_err()
        .to_string();
        assert!(error.contains("phoxal update"), "{error}");
        Ok(())
    }

    #[test]
    fn complete_store_errors_with_update_hint_for_a_missing_official() -> Result<()> {
        let _scratch = ScratchPhoxalHome::new()?;
        let project = tempfile::tempdir()?;
        fs::create_dir_all(project.path().join("model"))?;
        fs::write(project.path().join("model/structure.urdf"), "<robot/>")?;
        let mut resolved = resolved_robot()?;
        resolved.platform_runtimes = vec![platform_runtime("drive", ArtifactKind::Service)];
        let staged = stage_runtime_layout(project.path(), &resolved)?;

        let error = stage_complete_official_store(&staged, &resolved, |_, name| {
            panic!("no source override expected for {name}")
        })
        .unwrap_err()
        .to_string();
        assert!(error.contains("phoxal update"), "{error}");
        assert!(error.contains("phoxal-service-drive"), "{error}");
        Ok(())
    }

    #[test]
    fn complete_store_builds_source_overridden_dormant_officials() -> Result<()> {
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
        stage_complete_official_store(&staged, &resolved, |crate_dir, name| {
            assert_eq!(crate_dir, Path::new("services/drive"));
            assert_eq!(name, "drive");
            Ok(built.clone())
        })?;

        assert_eq!(
            fs::read_to_string(staged.join("bin/phoxal-service-drive"))?,
            "OVERRIDE"
        );
        Ok(())
    }

    #[test]
    fn stage_router_binary_links_only_the_router() -> Result<()> {
        let _scratch = ScratchPhoxalHome::new()?;
        let project = tempfile::tempdir()?;
        fs::create_dir_all(project.path().join("model"))?;
        fs::write(project.path().join("model/structure.urdf"), "<robot/>")?;
        let mut resolved = resolved_robot()?;
        resolved.platform_runtimes = vec![platform_runtime("drive", ArtifactKind::Service)];
        resolved.tools = vec![router_tool()];
        let staged = stage_runtime_layout(project.path(), &resolved)?;

        seed_vendored(
            &NativeArtifactDescriptor::from_tool(&resolved.tools[0])?.unwrap(),
            b"ROUTER",
        )?;
        stage_router_binary(&staged, &resolved, |_, name| {
            panic!("no source override expected for {name}")
        })?;

        assert_eq!(
            fs::read_to_string(staged.join("bin/phoxal-infrastructure-router"))?,
            "ROUTER"
        );
        // The router-only step never stages dormant services.
        assert!(!staged.join("bin/phoxal-service-drive").exists());
        Ok(())
    }
}
