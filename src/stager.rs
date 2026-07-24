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

use std::collections::BTreeMap;
use std::fs;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, ensure};

use phoxal_cli_core::artifacts::NativeArtifactDescriptor;
use phoxal_cli_core::project::launch_plan::{
    LaunchPlan, ParticipantExecution, ParticipantLaunchRecord, runtime_layout_dir,
};
use phoxal_cli_core::project::resolver::{ResolvedRobot, official_binary_name};
use phoxal_cli_core::project::suite::ArtifactKind;

use crate::supervisor::ParticipantSpec;

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
/// layout populate it with [`link_runtime_binaries`].
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

/// The canonical `bin/` file name every launched participant is staged under,
/// keyed by its launch participant id. `bin/` is an identity-keyed lookup
/// store: an official service built from a Cargo workspace override lands at the
/// SAME `bin/` name a vendored one would, so the loader's
/// [`RuntimeLayout::resolve_binary`](phoxal_cli_core::project::layout::RuntimeLayout::resolve_binary)
/// matches source-built and vendored binaries identically. The names computed
/// here are exactly the ones the loader derives from the compiled `robot.yaml`
/// and the CLI catalog.
#[must_use]
pub fn canonical_bin_names(plan: &LaunchPlan) -> BTreeMap<String, String> {
    plan.robots
        .iter()
        .flat_map(|robot| robot.participants.iter())
        .map(|participant| {
            (
                participant.launch.participant_id.clone(),
                canonical_binary_name(participant),
            )
        })
        .collect()
}

/// The canonical identity name one launched participant is stored under in
/// `bin/`, matching the loader's derivation from the compiled `robot.yaml`.
fn canonical_binary_name(participant: &ParticipantLaunchRecord) -> String {
    // Component drivers (source- or suite-sourced) are scoped to a component
    // instance and share one driver binary named by the component id, so every
    // instance of the same component resolves to a single `bin/` entry.
    if participant.launch.component_instance.is_some() {
        return official_binary_name(ArtifactKind::ComponentDriver, &participant.artifact_id);
    }
    match &participant.execution {
        // A user service is its own identity - the key it occupies in the
        // compiled `robot.yaml` `services` map.
        ParticipantExecution::UserService { .. } => participant.artifact_id.clone(),
        ParticipantExecution::OfficialTool { .. } => official_binary_name(
            ArtifactKind::Tool,
            tool_short_name(&participant.artifact_id),
        ),
        ParticipantExecution::SourceArtifact { kind, .. } if kind == "tool" => {
            official_binary_name(
                ArtifactKind::Tool,
                tool_short_name(&participant.artifact_id),
            )
        }
        ParticipantExecution::SourceArtifact { kind, .. } if kind == "simulator" => {
            official_binary_name(ArtifactKind::Simulator, &participant.artifact_id)
        }
        // An official service resolves to the same canonical service binary
        // whether it is vendored (`OfficialArtifact`) or built from a workspace
        // override (`SourceArtifact { kind: "service" }`).
        ParticipantExecution::OfficialArtifact { .. }
        | ParticipantExecution::SourceArtifact { .. } => {
            official_binary_name(ArtifactKind::Service, &participant.artifact_id)
        }
        ParticipantExecution::ComponentDriver { .. } => {
            official_binary_name(ArtifactKind::ComponentDriver, &participant.artifact_id)
        }
    }
}

/// The kind-stripped short name of an official tool artifact id
/// (`tool-bus` -> `bus`), matching the loader's catalog short name.
fn tool_short_name(artifact_id: &str) -> &str {
    artifact_id.strip_prefix("tool-").unwrap_or(artifact_id)
}

/// Hardlink (with a cross-filesystem copy fallback) every planned binary into
/// the staged `bin/` under its canonical identity name (from `names`, keyed by
/// participant id), then repoint each spec's `executable` at its staged entry.
/// `bin/` is a flat identity-keyed store: one canonical file per binary, so one
/// driver binary shared by several component instances is linked once, and a
/// source-overridden official lands at the same name the vendored one would.
/// `bin/` is cleared and recreated every pass, so it can never go stale. No
/// symlinks: the staged layout holds real file identities that keep working if
/// `target/` is later cleaned.
///
/// macOS `.app`-bundled executables are left untouched (still pointing into
/// their bundle) so the supervisor's `.app` materialization keeps working;
/// flattening a bundle into one `bin/` file would drop its `Contents/`. Native
/// Linux runtime binaries - the real deployment target - are always flattened.
pub fn link_runtime_binaries(
    staged_root: &Path,
    specs: &mut [ParticipantSpec],
    names: &BTreeMap<String, String>,
) -> Result<()> {
    let bin_dir = staged_root.join(BIN_DIR);
    remove_if_present(&bin_dir)?;
    fs::create_dir_all(&bin_dir)
        .with_context(|| format!("failed to create staged bin store {}", bin_dir.display()))?;

    for spec in specs {
        if macos_app_bundle_binary(&spec.executable).is_some() {
            continue;
        }
        if !spec.executable.is_file() {
            continue;
        }
        let canonical = names.get(&spec.id).with_context(|| {
            format!(
                "no canonical bin/ name for launched participant `{}`",
                spec.id
            )
        })?;
        let staged = bin_dir.join(canonical);
        link_or_copy(&spec.executable, &staged)?;
        spec.executable = staged;
    }
    Ok(())
}

/// Resolve a vendored official/tool binary from the project-local
/// `.phoxal/artifacts` store, failing with a "run `phoxal update`" error and
/// never touching the network when the store lacks it. Staging links from the
/// vendored store only; it never fetches. Used by the next slice's universal
/// loader; kept here so vendored resolution lives with the rest of the stager.
pub fn resolve_vendored_binary(descriptor: &NativeArtifactDescriptor) -> Result<PathBuf> {
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

/// The `.app` bundle root when `executable` lives inside a macOS
/// `Foo.app/Contents/MacOS/` bundle, else `None`.
fn macos_app_bundle_binary(executable: &Path) -> Option<&Path> {
    executable
        .ancestors()
        .find(|path| path.extension().is_some_and(|extension| extension == "app"))
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
        if component.assets.is_some() {
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
    use phoxal_cli_core::project::resolver::{
        ResolvedComponent, ResolvedComponentPackage, ResolvedComponentSource, ResolvedRobot,
        ResolvedUserRuntime,
    };
    use phoxal_cli_core::project::suite::ArtifactKind;
    use phoxal_cli_core::session::{ParticipantKind, ProcessKey, RobotKey};
    use std::os::unix::fs::MetadataExt;
    use std::time::Duration;

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

    fn name_map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(id, name)| ((*id).to_string(), (*name).to_string()))
            .collect()
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

    fn spec(name: &str, executable: PathBuf) -> ParticipantSpec {
        let robot = RobotKey::new("dev", "robot_v1");
        ParticipantSpec {
            key: ProcessKey::robot(robot.clone(), name),
            id: name.to_string(),
            kind: ParticipantKind::Service,
            executable,
            args: Vec::new(),
            cwd: None,
            env: Vec::new(),
            shutdown_grace: Duration::from_millis(0),
            process_group: true,
            note: None,
            bus_participant: true,
            readiness: ParticipantSpec::exact_liveliness_template(robot, name),
            startup_requirement: phoxal_cli_core::session::StartupRequirement::Required,
            runtime_failure: phoxal_cli_core::session::RuntimeFailurePolicy::StopProject,
            restart_policy: Default::default(),
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
    fn links_binaries_into_a_flat_bin_store_by_hardlink_identity() -> Result<()> {
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

        let mut specs = vec![spec("mission", built.clone())];
        link_runtime_binaries(&staged, &mut specs, &name_map(&[("mission", "mission")]))?;

        let staged_bin = staged.join("bin/mission");
        assert!(staged_bin.is_file());
        // Repointed at the flat store, not the cargo target path.
        assert_eq!(specs[0].executable, staged_bin);
        // Hardlink identity: same inode as the built artifact.
        assert_eq!(
            fs::metadata(&built)?.ino(),
            fs::metadata(&staged_bin)?.ino(),
            "bin/ entry must be a hardlink to the built artifact"
        );

        // A refresh after the artifact changes relinks - bin/ never goes stale.
        fs::remove_file(&built)?;
        fs::write(&built, "ELF-v2")?;
        let mut specs = vec![spec("mission", built.clone())];
        link_runtime_binaries(&staged, &mut specs, &name_map(&[("mission", "mission")]))?;
        assert_eq!(fs::read_to_string(staged.join("bin/mission"))?, "ELF-v2");
        assert_eq!(
            fs::metadata(&built)?.ino(),
            fs::metadata(staged.join("bin/mission"))?.ino()
        );
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

    #[test]
    fn macos_app_bundled_binaries_are_left_for_bundle_materialization() -> Result<()> {
        let _scratch = ScratchPhoxalHome::new()?;
        let project = tempfile::tempdir()?;
        fs::create_dir_all(project.path().join("model"))?;
        fs::write(project.path().join("model/structure.urdf"), "<robot/>")?;
        let resolved = resolved_robot()?;
        let staged = stage_runtime_layout(project.path(), &resolved)?;

        let bundle = project.path().join("Joypad.app/Contents/MacOS");
        fs::create_dir_all(&bundle)?;
        let bundled = bundle.join("joypad");
        fs::write(&bundled, "MACHO")?;

        let mut specs = vec![spec("tool-joypad", bundled.clone())];
        link_runtime_binaries(
            &staged,
            &mut specs,
            &name_map(&[("tool-joypad", "phoxal-tool-joypad")]),
        )?;
        // Bundle-internal binaries are never flattened into bin/.
        assert!(!staged.join("bin/joypad").exists());
        assert!(!staged.join("bin/phoxal-tool-joypad").exists());
        assert_eq!(specs[0].executable, bundled);
        Ok(())
    }

    #[test]
    fn canonical_binary_names_are_identity_keyed_across_sources() {
        // A vendored official service and a source-overridden one resolve to
        // the SAME canonical bin/ name, so the loader matches them identically.
        assert_eq!(
            canonical_binary_name(&launch_record(
                "drive",
                "drive",
                ParticipantExecution::OfficialArtifact {
                    artifact_ref: "ref".to_string()
                },
                None,
            )),
            "phoxal-service-drive"
        );
        assert_eq!(
            canonical_binary_name(&launch_record(
                "drive",
                "drive",
                ParticipantExecution::SourceArtifact {
                    kind: "service".to_string(),
                    crate_dir: PathBuf::from("services/drive"),
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
                    crate_dir: PathBuf::from("services/mission"),
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
                    artifact_ref: "ref".to_string()
                },
                None,
            )),
            "phoxal-tool-bus"
        );
        // A component driver is named by its component id and shared across
        // every instance - whether built from source or resolved from the suite.
        assert_eq!(
            canonical_binary_name(&launch_record(
                "left_drive",
                "ddsm115",
                ParticipantExecution::ComponentDriver {
                    crate_dir: PathBuf::from("components/ddsm115"),
                },
                Some("left_drive"),
            )),
            "phoxal-component-ddsm115"
        );
        assert_eq!(
            canonical_binary_name(&launch_record(
                "right_drive",
                "ddsm115",
                ParticipantExecution::OfficialArtifact {
                    artifact_ref: "ref".to_string()
                },
                Some("right_drive"),
            )),
            "phoxal-component-ddsm115"
        );
    }

    #[test]
    fn links_officials_and_shared_drivers_under_canonical_identity_names() -> Result<()> {
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

        let mut specs = vec![
            spec("drive", drive.clone()),
            spec("left_drive", driver.clone()),
            spec("right_drive", driver.clone()),
        ];
        link_runtime_binaries(
            &staged,
            &mut specs,
            &name_map(&[
                ("drive", "phoxal-service-drive"),
                ("left_drive", "phoxal-component-ddsm115"),
                ("right_drive", "phoxal-component-ddsm115"),
            ]),
        )?;

        // The source-overridden official lands at the vendored canonical name.
        let staged_drive = staged.join("bin/phoxal-service-drive");
        assert!(staged_drive.is_file());
        assert_eq!(specs[0].executable, staged_drive);
        // One driver binary, one bin/ entry, both instance specs repointed at it.
        let shared = staged.join("bin/phoxal-component-ddsm115");
        assert!(shared.is_file());
        assert_eq!(specs[1].executable, shared);
        assert_eq!(specs[2].executable, shared);
        assert_eq!(fs::metadata(&driver)?.ino(), fs::metadata(&shared)?.ino());
        Ok(())
    }

    #[test]
    fn linking_a_participant_absent_from_the_name_map_is_an_error() -> Result<()> {
        let _scratch = ScratchPhoxalHome::new()?;
        let project = tempfile::tempdir()?;
        fs::create_dir_all(project.path().join("model"))?;
        fs::write(project.path().join("model/structure.urdf"), "<robot/>")?;
        let resolved = resolved_robot()?;
        let staged = stage_runtime_layout(project.path(), &resolved)?;

        let built = project.path().join("target/debug/mission");
        fs::create_dir_all(built.parent().unwrap())?;
        fs::write(&built, "ELF")?;
        let mut specs = vec![spec("mission", built)];
        let error = link_runtime_binaries(&staged, &mut specs, &name_map(&[]))
            .unwrap_err()
            .to_string();
        assert!(error.contains("mission"), "{error}");
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
            assets: Some(ResolvedComponentPackage {
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
}
