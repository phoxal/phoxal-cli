//! Participants responsibilities for run.
//!
//! Execution resolves every participant binary from the staged runtime layout's
//! flat `bin/` store and inspects it off-disk (host-architecture check plus
//! embedded metadata) before it is ever spawned. There is no cargo-target /
//! vendored-store lookup and no graceful "pending" board note at launch: a
//! participant whose binary the staging step could not produce, or whose staged
//! binary is built for a foreign architecture, is a HARD startup failure naming
//! the required identity (#936). Staging (`crate::stage`) is the only code that
//! knows about `cargo install` materialization; this module only reads what
//! staging produced.

use super::report::DriverPolicy;
use crate::PreparedParticipant;
use crate::build::cargo::{SourceArtifacts, device_missing_note, missing_device_path};
use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use phoxal_cli_core::check::participant_metadata::inspect_selected_binary;
use phoxal_cli_core::check::source::SourceParticipant;
use phoxal_cli_core::check::source::SourceParticipantKind;
use phoxal_cli_core::project::catalog::ArtifactKind;
use phoxal_cli_core::project::launch_plan::LaunchPlan;
use phoxal_cli_core::project::launch_plan::ParticipantExecution;
use phoxal_cli_core::project::launch_plan::ParticipantLaunchRecord;
#[cfg(test)]
use phoxal_cli_core::project::layout::DriverSelection;
use phoxal_cli_core::project::layout::RuntimeLayout;
use phoxal_cli_core::project::resolver::BundlePlan;
use phoxal_cli_core::project::resolver::ResolvedPlatformRuntime;
use phoxal_cli_core::project::resolver::official_binary_name;
use phoxal_cli_core::runtime::ParticipantKind;
use phoxal_cli_core::runtime::ParticipantSpec;
use phoxal_cli_core::runtime::launch::encode_participant_env;
use phoxal_cli_core::runtime::{ParticipantState, ProcessKey, RobotKey};
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DriverDecision {
    Launch,
    Degraded(String),
}

/// The staging-side record of source crate directories, keyed by launch
/// participant id, that accompanies a plan built from a source project. The
/// source-free plan (#936) never names a crate directory, so cargo rebuild and
/// process cwd for the user services and workspace-built component drivers a
/// source project owns are recovered from this map instead. It is empty for a
/// plan built from an extracted bundle, which has no source at all.
pub(crate) fn source_dirs_by_participant(
    source_participants: &[phoxal_cli_core::check::source::SourceParticipant],
) -> BTreeMap<String, PathBuf> {
    source_participants
        .iter()
        .map(|participant| (participant.name.clone(), participant.crate_dir.clone()))
        .collect()
}

/// Populate the staged layout's flat `bin/` store with every binary the loader
/// requires, so [`RuntimeLayout::construct_plan`] can inspect the complete set
/// off-disk before any process launches (#936). This is the staging-side
/// counterpart of the execution path: it is the only code that resolves source,
/// keyed by the resolved graph (`BundlePlan`) and its source-participant
/// records, never by a plan.
///
/// It links every source-built user service and workspace/path-overridden
/// component driver. Registry packages and source-overridden officials are
/// materialized together by `stage::materialize_candidate_store` before this
/// source-only pass. After both passes, `bin/` is the complete lookup store an
/// extracted bundle would carry - the loader resolves every required runtime
/// from it with no source present.
///
pub(crate) fn stage_complete_bin_store(
    staged_root: &Path,
    source_participants: &[SourceParticipant],
    source_artifacts: &SourceArtifacts,
) -> Result<()> {
    let mut staged_names = BTreeSet::new();
    // Source-built user services and workspace/path-overridden component
    // drivers. Official-service/simulator source overrides are materialized by
    // the candidate-wide planner, so they are skipped here.
    for participant in source_participants {
        let binary_name = match participant.kind {
            SourceParticipantKind::UserService => participant.name.clone(),
            SourceParticipantKind::ComponentDriver => official_binary_name(
                ArtifactKind::ComponentDriver,
                &participant.expected_artifact_id,
            ),
            SourceParticipantKind::OfficialService | SourceParticipantKind::Simulator => {
                continue;
            }
        };
        if !staged_names.insert(binary_name.clone()) {
            continue;
        }
        let built = source_artifacts.binary(participant)?;
        crate::stage::stage_named_binary(staged_root, &binary_name, built)?;
    }
    Ok(())
}

/// Build the participant specs for a launch plan whose binaries already live in
/// the staged layout's flat `bin/` store, resolving every executable directly
/// from `bin/` with no source, Cargo, or resolved graph (#936). This is the ONE
/// execution-side spec builder for a staged runtime layout - a source project's
/// `.phoxal/bundle/` (after [`stage_complete_bin_store`] populated and
/// [`crate::load::layout::validate_layout_plan`] validated it) or an extracted
/// `build.phoxal`. Because it reads the already-validated `bin/` and never
/// rebuilds, the executed bytes are exactly the validated bytes - there is no
/// second resolve/rebuild pass that could diverge (#936, finding 3).
///
/// `cwd_for` supplies the source-only working directory the source-free plan no
/// longer carries: the source run passes [`source_cwd`] so a participant built
/// from local source runs from its crate directory, and an extracted-bundle run
/// passes a closure returning `None` (a bundle has no source). Board
/// classification, the component-driver missing-device check (read straight
/// from compiler-owned participant declarations), and readiness/env/policy encoding are
/// shared by both. Driver policy needs no gate here: the plan constructor
/// already excluded non-selected drivers, so every driver in the plan launches.
pub(crate) fn build_layout_specs(
    plan: &LaunchPlan,
    layout: &RuntimeLayout,
    cwd_for: &dyn Fn(&ParticipantLaunchRecord) -> Option<PathBuf>,
) -> Result<Vec<PreparedParticipant>> {
    let mut prepared = Vec::new();
    let bin_dir = layout.bin_dir();
    for robot in &plan.robots {
        let robot_key = RobotKey::new(&robot.namespace, &robot.id);
        for participant in &robot.participants {
            let id = participant.launch.participant_id.clone();
            let key = ProcessKey::robot(robot_key.clone(), &id);
            let (kind, base_local) = participant_kind(&participant.execution);
            // Board-only staging provenance (#936, finding 10): a source-
            // overridden official runs from the project workspace, so it
            // has a source cwd here even though its source-free `execution` is
            // byte-identical to a vendored one. Reflect that on the board (local
            // = runs from the robot's own code) WITHOUT touching the plan, which
            // stays identical across origins. An extracted layout supplies no
            // cwd, so its officials correctly stay origin-unknown (vendored).
            let cwd = cwd_for(participant);
            let source_overridden_official = cwd.is_some()
                && matches!(
                    participant.execution,
                    ParticipantExecution::OfficialArtifact { .. }
                );
            let local = base_local || source_overridden_official;
            let mut note = source_overridden_official
                .then(|| "source-override: built from the project workspace".to_string());
            if matches!(
                participant.execution,
                ParticipantExecution::ComponentDriver { .. }
            ) && let Some(note) = layout_device_missing_note(layout, &id)?
            {
                prepared.push(PreparedParticipant {
                    key,
                    id,
                    kind,
                    robot: Some(robot_key.clone()),
                    local,
                    startup_requirement: participant.startup_requirement,
                    initial_state: ParticipantState::Failed,
                    note: Some(note),
                    launch: None,
                });
                continue;
            }
            let executable = bin_dir.join(participant.execution.binary_name());
            inspect_selected_binary(&executable).with_context(|| {
                format!(
                    "failed to inspect staged runtime `{}` at {}",
                    id,
                    executable.display()
                )
            })?;
            let launch = participant_spec(participant, &robot_key, kind, executable, cwd)?;
            if note.is_none() {
                note = launch.note.clone();
            }
            prepared.push(PreparedParticipant {
                key,
                id,
                kind,
                robot: Some(robot_key.clone()),
                local,
                startup_requirement: participant.startup_requirement,
                initial_state: ParticipantState::Starting,
                note,
                launch: Some(launch),
            });
        }
    }
    Ok(prepared)
}

/// The missing-device board note for a driver participant in a compiled
/// authored source, computed directly from the layout's canonical model (no resolved
/// graph). Mirrors [`device_missing_note`], which reads the same connection
/// config off a `BundlePlan`.
fn layout_device_missing_note(
    layout: &RuntimeLayout,
    participant_id: &str,
) -> Result<Option<String>> {
    let Some(participant) = layout.participants().iter().find(|participant| {
        participant.kind == phoxal_manifest::ParticipantKind::Driver
            && participant.component_instance.as_deref() == Some(participant_id)
    }) else {
        return Ok(None);
    };
    let Some(config) = participant.config.clone() else {
        return Ok(None);
    };
    let driver: phoxal_manifest::source::robot::v0::DriverConfig =
        serde_json::from_value(config)
            .with_context(|| format!("compiled driver config for '{participant_id}' is invalid"))?;
    let Some(missing) = missing_device_path(&driver.connection) else {
        return Ok(None);
    };
    Ok(Some(format!(
        "DeviceMissing: {missing} for driver {participant_id}"
    )))
}

pub(crate) fn prepare_robot_participants(
    plan: &LaunchPlan,
    resolved: &BundlePlan,
    source_dirs: &BTreeMap<String, PathBuf>,
    source_artifacts: &SourceArtifacts,
    staged_root: &Path,
    driver_policy: &DriverPolicy,
) -> Result<Vec<PreparedParticipant>> {
    let mut prepared = Vec::new();
    let official_by_name = official_runtimes_by_name(resolved);
    for robot in &plan.robots {
        let robot_key = RobotKey::new(&robot.namespace, &robot.id);
        for participant in &robot.participants {
            let id = participant.launch.participant_id.clone();
            let key = ProcessKey::robot(robot_key.clone(), &id);
            let (kind, local) = participant_kind(&participant.execution);
            // Component-driver launch gating (bench subset, missing
            // device) is a board/policy decision that precedes any binary
            // resolution: a gated-out driver never needs its binary staged.
            if matches!(
                participant.execution,
                ParticipantExecution::ComponentDriver { .. }
            ) {
                match driver_policy.decision(&id) {
                    DriverDecision::Degraded(note) => {
                        prepared.push(PreparedParticipant {
                            key,
                            id,
                            kind,
                            robot: Some(robot_key.clone()),
                            local,
                            startup_requirement: participant.startup_requirement,
                            initial_state: ParticipantState::Degraded,
                            note: Some(note),
                            launch: None,
                        });
                        continue;
                    }
                    DriverDecision::Launch => {}
                }
                if let Some(note) = device_missing_note(resolved, &id) {
                    prepared.push(PreparedParticipant {
                        key,
                        id,
                        kind,
                        robot: Some(robot_key.clone()),
                        local,
                        startup_requirement: participant.startup_requirement,
                        initial_state: ParticipantState::Failed,
                        note: Some(note),
                        launch: None,
                    });
                    continue;
                }
            }
            let source = resolve_participant_source(
                staged_root,
                participant,
                &official_by_name,
                source_dirs,
                source_artifacts,
            )?;
            let executable = stage_and_inspect(staged_root, participant, &source)?;
            let cwd = source_cwd(participant, resolved, source_dirs);
            let launch = participant_spec(participant, &robot_key, kind, executable, cwd)?;
            prepared.push(PreparedParticipant {
                key,
                id,
                kind,
                robot: Some(robot_key.clone()),
                local,
                startup_requirement: participant.startup_requirement,
                initial_state: ParticipantState::Starting,
                note: launch.note.clone(),
                launch: Some(launch),
            });
        }
    }
    Ok(prepared)
}

/// The board `ParticipantKind` plus whether the participant runs from local
/// (user/robot-owned) code, for a participant's source-free `execution` (#936).
/// The role alone decides both here: officials are framework binaries
/// (`local = false`), user services and component drivers are the robot's own
/// code (`local = true`). An official the robot overrides in its Cargo workspace
/// still resolves to the one official `bin/` entry, so the PLAN cannot and does
/// not distinguish "overridden" from "vendored" - it stays byte-identical across
/// origins. The board, however, refines this `local` bit from the source-side
/// staging origin (a source cwd) so a source-overridden official is shown as
/// local with a provenance note; an extracted layout has no such origin and its
/// officials stay vendored (#936, finding 10). See [`build_layout_specs`].
pub(crate) fn participant_kind(execution: &ParticipantExecution) -> (ParticipantKind, bool) {
    match execution {
        ParticipantExecution::OfficialArtifact { .. } => (ParticipantKind::Service, false),
        ParticipantExecution::UserService { .. } => (ParticipantKind::Service, true),
        ParticipantExecution::ComponentDriver { .. } => (ParticipantKind::Driver, true),
    }
}

/// Every official platform runtime the loader may need to resolve, keyed by
/// its launch identity: the services and simulators in `platform_runtimes`
/// plus the registry-sourced component drivers carried on
/// `components[].driver.registry_runtime` (a registry driver projects onto
/// the same `ResolvedPlatformRuntime` shape and is keyed by its component
/// id). Source-sourced drivers are not here - they build from their crate
/// through the source-execution path.
fn official_runtimes_by_name(resolved: &BundlePlan) -> BTreeMap<&str, &ResolvedPlatformRuntime> {
    resolved
        .platform_runtimes
        .iter()
        .chain(
            resolved
                .components
                .iter()
                .filter_map(|component| component.driver.as_ref())
                .filter_map(|driver| driver.registry_runtime()),
        )
        .map(|runtime| (runtime.name.as_str(), runtime))
        .collect()
}

/// Resolve the binary one launched participant runs from, so it can be staged
/// into the layout's `bin/`. The source-free plan (#936) names only the
/// participant's role and `bin/` binary, so where its bytes come from is
/// recovered here from the resolved graph and the staging-side `source_dirs`
/// record: a user service and a workspace-built component driver build
/// through `cargo` from their crate directory; an official artifact or
/// registry-provided driver materializes via `cargo install`, straight into
/// `staged_root/bin/`. The candidate-wide materialization pass has already
/// completed before this function runs; every registry path therefore only
/// reads that candidate and hard-fails if its required entry is absent.
fn resolve_participant_source(
    staged_root: &Path,
    participant: &ParticipantLaunchRecord,
    official_by_name: &BTreeMap<&str, &ResolvedPlatformRuntime>,
    source_dirs: &BTreeMap<String, PathBuf>,
    source_artifacts: &SourceArtifacts,
) -> Result<PathBuf> {
    let id = &participant.launch.participant_id;
    let prepared = staged_root
        .join("bin")
        .join(participant.execution.binary_name());
    if prepared.is_file() {
        // Candidate-wide materialization and native source staging are the
        // only mutation owners. Execution preparation reuses that exact entry
        // instead of removing and relinking an already-prepared override.
        return Ok(prepared);
    }
    match &participant.execution {
        ParticipantExecution::UserService { .. } => {
            source_dirs.get(id).ok_or_else(|| {
                anyhow!("staged plan is missing the source crate directory for user runtime {id}")
            })?;
            source_artifacts.binary_named(id).map(PathBuf::from)
        }
        ParticipantExecution::ComponentDriver { .. } => {
            // A workspace-built driver has a crate directory in the staging
            // record; a registry-provided one does not and materializes via
            // `cargo install`, keyed by its component id.
            if source_dirs.contains_key(id) {
                return source_artifacts.binary_named(id).map(PathBuf::from);
            }
            let runtime = official_by_name
                .get(participant.artifact_id.as_str())
                .ok_or_else(|| {
                    anyhow!(
                        "resolved graph is missing component driver {}",
                        participant.artifact_id
                    )
                })?;
            let staged = staged_root
                .join("bin")
                .join(official_binary_name(runtime.kind, &runtime.name));
            anyhow::ensure!(
                staged.is_file(),
                "candidate-wide preparation did not materialize component driver '{}' at {}",
                runtime.name,
                staged.display()
            );
            Ok(staged)
        }
        ParticipantExecution::OfficialArtifact { .. } => {
            let runtime = official_by_name
                .get(participant.artifact_id.as_str())
                .ok_or_else(|| {
                    anyhow!(
                        "resolved graph is missing official artifact {}",
                        participant.artifact_id
                    )
                })?;
            if runtime.path_override.is_some() {
                return source_artifacts
                    .binary_named(&runtime.name)
                    .map(PathBuf::from);
            }
            let staged = staged_root
                .join("bin")
                .join(official_binary_name(runtime.kind, &runtime.name));
            anyhow::ensure!(
                staged.is_file(),
                "candidate-wide preparation did not materialize official artifact '{}' at {}",
                runtime.name,
                staged.display()
            );
            Ok(staged)
        }
    }
}

/// Stage one participant's resolved source binary into the layout's flat `bin/`
/// and inspect the staged entry off-disk: verify it is executable on the host
/// architecture and read its embedded metadata, never running it. A missing or
/// foreign-architecture binary is a hard startup failure naming the identity.
fn stage_and_inspect(
    staged_root: &Path,
    participant: &ParticipantLaunchRecord,
    source: &Path,
) -> Result<PathBuf> {
    let staged = crate::stage::stage_participant_binary(staged_root, participant, source)?;
    inspect_selected_binary(&staged).with_context(|| {
        format!(
            "failed to inspect staged runtime `{}` at {}",
            participant.launch.participant_id,
            staged.display()
        )
    })?;
    Ok(staged)
}

/// The working directory a launched participant runs in: a participant built
/// from source (a user service, a workspace-built driver, or an official the
/// robot overrides in its Cargo workspace) runs from its crate directory so
/// relative asset paths resolve; a vendored official runs from the layout with
/// no crate context. The crate directory comes from the staging-side record
/// (user services / workspace drivers) or the resolved graph's path override
/// (overridden officials).
pub(crate) fn source_cwd(
    participant: &ParticipantLaunchRecord,
    resolved: &BundlePlan,
    source_dirs: &BTreeMap<String, PathBuf>,
) -> Option<PathBuf> {
    let id = &participant.launch.participant_id;
    match &participant.execution {
        ParticipantExecution::UserService { .. } | ParticipantExecution::ComponentDriver { .. } => {
            source_dirs.get(id).cloned()
        }
        ParticipantExecution::OfficialArtifact { .. } => resolved
            .platform_runtimes
            .iter()
            .find(|runtime| runtime.name == participant.artifact_id)
            .and_then(|runtime| runtime.path_override.clone()),
    }
}

/// Assemble the `ParticipantSpec` the supervisor spawns: the staged executable
/// plus the launch env/readiness/policies carried by the plan record.
fn participant_spec(
    participant: &ParticipantLaunchRecord,
    robot_key: &RobotKey,
    kind: ParticipantKind,
    executable: PathBuf,
    cwd: Option<PathBuf>,
) -> Result<ParticipantSpec> {
    let id = participant.launch.participant_id.clone();
    let env = encode_participant_env(&participant.launch)?;
    Ok(ParticipantSpec {
        key: ProcessKey::robot(robot_key.clone(), &id),
        id,
        kind,
        executable,
        args: Vec::new(),
        cwd,
        env: env.spawn_env(),
        shutdown_grace: Duration::from_millis(participant.launch.shutdown_grace_ms),
        process_group: true,
        note: None,
        bus_participant: true,
        readiness: ParticipantSpec::exact_liveliness_template(
            robot_key.clone(),
            &participant.launch.participant_id,
        ),
        startup_requirement: participant.startup_requirement,
        runtime_failure: participant.runtime_failure,
        restart_policy: Default::default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use phoxal_cli_core::check::participant_metadata::host_architecture;
    use phoxal_cli_core::identity::{ExecutionId, ProducerId};
    use phoxal_cli_core::project::launch_plan::RunIdentity;
    use phoxal_cli_core::runtime::RuntimeFailurePolicy;
    use phoxal_cli_core::runtime::StartupRequirement;
    use phoxal_runtime_contract::{
        BusProfile, ClockMode, DEFAULT_SHUTDOWN_GRACE_MS, ParticipantLaunch,
    };

    /// Synthesize a host-format object of a given architecture carrying the
    /// phoxal metadata section, so inspection is exercised against real object
    /// shapes without building a binary (mirrors the loader's own synthesis).
    /// `id` is the binary's own declared participant id. A staged binary's id
    /// must match the required runtime's identity, so a
    /// caller staging more than one required runtime must give each its own
    /// matching id rather than reusing a fixed payload).
    fn synthesize_binary_with_id(arch: object::Architecture, id: &str, kind: &str) -> Vec<u8> {
        use object::write::Object;
        let format = phoxal_cli_core::check::participant_metadata::host_binary_format();
        let (segment, name): (&[u8], &[u8]) = match format {
            object::BinaryFormat::MachO => (b"__DATA", b"__phoxal_meta"),
            _ => (b"", b".phoxal_meta"),
        };
        let mut obj = Object::new(format, arch, object::Endianness::Little);
        let section = obj.add_section(
            segment.to_vec(),
            name.to_vec(),
            object::SectionKind::ReadOnlyData,
        );
        let payload = format!(
            r#"{{"schema":"phoxal/participant-metadata/v0","id":"{id}","kind":"{kind}","config_schema":{{"type":"null"}}}}"#
        );
        obj.append_section_data(section, payload.as_bytes(), 1);
        obj.write().expect("synthesize object file")
    }

    /// [`synthesize_binary_with_id`] for a fixture whose only participant is
    /// `mission`.
    fn synthesize_binary(arch: object::Architecture) -> Vec<u8> {
        synthesize_binary_with_id(arch, "mission", "service")
    }

    fn user_service_record(id: &str) -> ParticipantLaunchRecord {
        ParticipantLaunchRecord {
            artifact_id: id.to_string(),
            execution: ParticipantExecution::UserService {
                binary_name: id.to_string(),
            },
            launch: ParticipantLaunch {
                participant_id: id.to_string(),
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
                bundle_root: None,
                component_instance: None,
                shutdown_grace_ms: DEFAULT_SHUTDOWN_GRACE_MS,
            },
            startup_requirement: StartupRequirement::Required,
            runtime_failure: RuntimeFailurePolicy::StopProject,
        }
    }

    #[test]
    fn a_staged_participant_binary_resolves_from_bin_under_the_layout() -> Result<()> {
        let staged = tempfile::tempdir()?;
        let source = tempfile::tempdir()?;
        let src_bin = source.path().join("mission");
        std::fs::write(&src_bin, synthesize_binary(host_architecture()))?;

        let record = user_service_record("mission");
        let executable = stage_and_inspect(staged.path(), &record, &src_bin)?;

        // The launched executable is the flat `bin/` entry under the layout,
        // not the source binary the staging step resolved.
        assert_eq!(executable, staged.path().join("bin/mission"));
        assert!(executable.starts_with(staged.path().join("bin")));
        assert!(executable.is_file());
        Ok(())
    }

    #[test]
    fn a_foreign_arch_staged_binary_fails_precisely_naming_the_identity() -> Result<()> {
        let staged = tempfile::tempdir()?;
        let source = tempfile::tempdir()?;
        let src_bin = source.path().join("mission");
        let foreign = if host_architecture() == object::Architecture::X86_64 {
            object::Architecture::Aarch64
        } else {
            object::Architecture::X86_64
        };
        std::fs::write(&src_bin, synthesize_binary(foreign))?;

        let record = user_service_record("mission");
        let error = format!(
            "{:#}",
            stage_and_inspect(staged.path(), &record, &src_bin)
                .expect_err("a foreign-arch staged binary must be rejected")
        );
        assert!(error.contains("mission"), "{error}");
        assert!(error.contains("built for"), "{error}");
        Ok(())
    }

    #[test]
    fn malformed_compiled_driver_config_fails_instead_of_hiding_device_state() -> Result<()> {
        let dir = tempfile::tempdir()?;
        crate::stage::write_test_layout(dir.path(), LAYOUT_ROBOT_YAML)?;
        let participants_path = dir
            .path()
            .join(phoxal_cli_core::project::layout::ASSETS_DIR)
            .join(phoxal_cli_core::project::layout::PARTICIPANTS_ASSET);
        let mut participants =
            phoxal_cli_core::project::layout::decode_participants(&participants_path)?;
        participants.push(phoxal_manifest::Participant {
            id: "wheel".to_string(),
            kind: phoxal_manifest::ParticipantKind::Driver,
            component_instance: Some("wheel".to_string()),
            config: Some(serde_json::json!({"connection": {"type": "serial"}})),
        });
        std::fs::write(
            &participants_path,
            serde_json::to_vec_pretty(&serde_json::json!({
                "schema": "phoxal/participants/v0",
                "participants": participants,
            }))?,
        )?;
        let error = RuntimeLayout::open(dir.path())
            .expect_err("invalid typed driver config must fail while opening the layout")
            .to_string();
        assert!(error.contains("invalid typed config"), "{error}");
        Ok(())
    }

    const LAYOUT_ROBOT_YAML: &str = r#"schema: robot/v0
robot:
  id: testbot
  namespace: dev
  motion_limits:
    max_linear_speed_mps: 0.6
    max_angular_speed_radps: 2.0
  kinematic:
    kind: omnidirectional
    actuators: []
    encoders: []
  components: {}
services:
  mission:
    config:
      speed: 1
"#;

    /// The layout execution path reads only the staged `bin/` store: every
    /// participant's executable is the flat `bin/<binary_name>` entry, resolved
    /// with no source, Cargo, resolved graph, or materialization state (#936,
    /// organization#951 WS4). A staged layout that carries a stray `.phoxal/`
    /// subdirectory proves the layout path never consults anything there.
    #[test]
    fn layout_specs_resolve_every_executable_from_bin_with_no_other_state() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let root = dir.path();
        crate::stage::write_test_layout(root, LAYOUT_ROBOT_YAML)?;
        let bin = root.join("bin");
        // Stray `.phoxal/` state the layout path must never touch: if it were
        // consulted the run would depend on it, defeating the bundle guarantee.
        std::fs::create_dir_all(root.join(".phoxal/stray"))?;

        let layout = RuntimeLayout::open(root)?;
        for required in layout.required_runtimes(&DriverSelection::All) {
            std::fs::write(
                bin.join(&required.binary_name),
                synthesize_binary_with_id(
                    host_architecture(),
                    &required.identity,
                    match required.kind {
                        phoxal_cli_core::project::layout::RequiredRuntimeKind::OfficialService
                        | phoxal_cli_core::project::layout::RequiredRuntimeKind::UserService => {
                            "service"
                        }
                        phoxal_cli_core::project::layout::RequiredRuntimeKind::ComponentDriver => {
                            "driver"
                        }
                    },
                ),
            )?;
        }

        let plan = RuntimeLayout::construct_plan(
            root,
            &phoxal_cli_core::project::layout::PlanOptions::default(),
            RunIdentity::default(),
        )?
        .plan;
        let prepared = build_layout_specs(&plan, &layout, &|_| None)?;
        let specs = prepared
            .iter()
            .filter_map(|participant| participant.launch.as_ref())
            .collect::<Vec<_>>();

        assert!(
            !specs.is_empty(),
            "the layout must produce launchable specs"
        );
        for spec in &specs {
            assert!(
                spec.executable.starts_with(&bin),
                "every executable must resolve from bin/: {}",
                spec.executable.display()
            );
            assert!(spec.executable.is_file(), "{}", spec.executable.display());
            assert!(
                spec.cwd.is_none(),
                "a staged layout participant has no source crate cwd"
            );
        }
        // The user service is present, proving compiled participant declarations
        // join the plan alongside the officials.
        assert!(
            specs.iter().any(|spec| spec.id == "mission"),
            "the user service `mission` must be launchable from the layout"
        );
        Ok(())
    }

    /// Board-only staging provenance (#936, finding 10): when the source path
    /// supplies a cwd for an official (a source override), the board marks that
    /// official `local` and carries a source-override note, while a vendored
    /// official (no cwd) stays `local = false`. The launch plan is untouched
    /// either way - only the board metadata differs.
    #[test]
    fn source_overridden_officials_are_marked_local_on_the_board() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let root = dir.path();
        crate::stage::write_test_layout(root, LAYOUT_ROBOT_YAML)?;
        let bin = root.join("bin");

        let layout = RuntimeLayout::open(root)?;
        for required in layout.required_runtimes(&DriverSelection::All) {
            std::fs::write(
                bin.join(&required.binary_name),
                synthesize_binary_with_id(
                    host_architecture(),
                    &required.identity,
                    match required.kind {
                        phoxal_cli_core::project::layout::RequiredRuntimeKind::OfficialService
                        | phoxal_cli_core::project::layout::RequiredRuntimeKind::UserService => {
                            "service"
                        }
                        phoxal_cli_core::project::layout::RequiredRuntimeKind::ComponentDriver => {
                            "driver"
                        }
                    },
                ),
            )?;
        }

        let plan = RuntimeLayout::construct_plan(
            root,
            &phoxal_cli_core::project::layout::PlanOptions::default(),
            RunIdentity::default(),
        )?
        .plan;
        // Pick one official artifact from the plan and pretend the project
        // overrides it in its workspace (a source cwd).
        let overridden = plan
            .robots
            .iter()
            .flat_map(|robot| &robot.participants)
            .find(|participant| {
                matches!(
                    participant.execution,
                    ParticipantExecution::OfficialArtifact { .. }
                )
            })
            .map(|participant| participant.launch.participant_id.clone())
            .expect("the plan has at least one official artifact");
        let overridden_cwd = overridden.clone();
        let cwd_for = move |participant: &ParticipantLaunchRecord| {
            (participant.launch.participant_id == overridden_cwd)
                .then(|| root.join("services").join(&overridden_cwd))
        };

        let prepared = build_layout_specs(&plan, &layout, &cwd_for)?;
        let status = prepared
            .iter()
            .find(|participant| participant.id == overridden)
            .expect("the overridden official is on the board");
        assert!(
            status.local,
            "a source-overridden official must be marked local on the board"
        );
        assert!(
            status
                .note
                .as_deref()
                .is_some_and(|note| note.contains("source-override")),
            "a source-overridden official must carry a provenance note: {:?}",
            status.note
        );
        Ok(())
    }
}
