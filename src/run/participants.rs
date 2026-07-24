//! Participants responsibilities for run.
//!
//! Execution resolves every participant binary from the staged runtime layout's
//! flat `bin/` store and inspects it off-disk (host-architecture check plus
//! embedded metadata) before it is ever spawned. There is no cargo-target /
//! vendored-store lookup and no graceful "pending" board note at launch: a
//! participant whose binary the staging step could not produce, or whose staged
//! binary is built for a foreign architecture, is a HARD startup failure naming
//! the required identity (#936). Staging (`crate::stager`) is the only code that
//! knows about Cargo and the vendored artifact store; this module only reads
//! what staging produced.

use super::{DriverPolicy, build_source_binary, device_missing_note};
use crate::supervisor::BoardBackend;
use crate::supervisor::ParticipantSpec;
use crate::supervisor::ParticipantState;
use crate::supervisor::ParticipantStatus;
use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use phoxal_cli_core::check::participant_metadata::inspect_selected_binary;
use phoxal_cli_core::project::launch_plan::LaunchOwnership;
use phoxal_cli_core::project::launch_plan::LaunchPlan;
use phoxal_cli_core::project::launch_plan::ParticipantExecution;
use phoxal_cli_core::project::launch_plan::ParticipantLaunchRecord;
use phoxal_cli_core::project::resolver::ResolvedPlatformRuntime;
use phoxal_cli_core::project::resolver::ResolvedRobot;
use phoxal_cli_core::session::ParticipantKind;
use phoxal_cli_core::session::launch_env::{encode_participant_env, encode_tool_env};
use phoxal_cli_core::session::stores::telemetry::RobotScope;
use phoxal_cli_core::session::{ProcessKey, RobotKey, StartupRequirement};
use std::collections::BTreeMap;
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

#[allow(clippy::too_many_arguments)]
pub(crate) fn prepare_robot_participants(
    plan: &LaunchPlan,
    resolved: &ResolvedRobot,
    source_dirs: &BTreeMap<String, PathBuf>,
    staged_root: &Path,
    driver_policy: &DriverPolicy,
    board: &BoardBackend,
    specs: &mut Vec<ParticipantSpec>,
    ui: &crate::Ui,
) -> Result<()> {
    let official_by_name = official_runtimes_by_name(resolved);
    for robot in &plan.robots {
        let robot_key = RobotKey::new(&robot.namespace, &robot.id);
        let scope = RobotScope {
            namespace: robot.namespace.clone(),
            robot_id: robot.id.clone(),
        };
        for participant in &robot.participants {
            let id = participant.launch.participant_id.clone();
            let key = ProcessKey::robot(robot_key.clone(), &id);
            let (kind, local) = participant_kind(&participant.execution);
            if participant.launch_ownership == LaunchOwnership::SimulationManaged {
                // Webots (via the supervisor) owns this participant's
                // lifecycle - the CLI never spawns or restarts it, and has no
                // process to poll for readiness. It still satisfies the graph
                // proof and appears on the board, starting `Starting`, not
                // `Ready`: OBSERVED readiness comes from its own stable bus
                // Liveliness token, driven by `BoardBackend::record_presence`
                // once the history-enabled observer is running. A
                // controller/supervisor Webots never launches, or that fails
                // before its own `#[setup]` completes, therefore never reaches
                // `Ready`; the staged startup wait detects that omission.
                // Disappearance after startup remains observational presence
                // state and never becomes synthesized process failure
                // authority.
                // `crate::simulation` renders its controllerArgs into the
                // staged world instead of a `ParticipantSpec` (Part 5).
                let mut status = ParticipantStatus::new(&id, kind, ParticipantState::Starting)
                    .with_local(local)
                    .with_scope(scope.clone());
                status.note = Some(
                    "SimulationManaged: launched by Webots via the supervisor, not the CLI supervisor"
                        .to_string(),
                );
                register_simulation_managed_participant(
                    board,
                    key,
                    status,
                    participant.launch.incarnation,
                    participant.startup_requirement,
                );
                continue;
            }
            board.upsert_process(
                key.clone(),
                ParticipantStatus::new(&id, kind, ParticipantState::Starting)
                    .with_local(local)
                    .with_scope(scope.clone()),
                participant.startup_requirement,
            );
            // Component-driver launch gating (bench subset, macOS, missing
            // device) is a board/policy decision that precedes any binary
            // resolution: a gated-out driver never needs its binary staged.
            if matches!(
                participant.execution,
                ParticipantExecution::ComponentDriver { .. }
            ) {
                match driver_policy.decision(&id) {
                    DriverDecision::Degraded(note) => {
                        board.set_state(&key, ParticipantState::Degraded, Some(note));
                        continue;
                    }
                    DriverDecision::Launch => {}
                }
                if cfg!(target_os = "macos") {
                    board.set_state(
                        &key,
                        ParticipantState::Failed,
                        Some("DriverUnsupported: component driver binaries are Linux-only on macOS (D21)".to_string()),
                    );
                    continue;
                }
                if let Some(note) = device_missing_note(resolved, &id) {
                    board.set_state(&key, ParticipantState::Failed, Some(note));
                    continue;
                }
            }
            let source = resolve_participant_source(
                participant,
                resolved,
                &official_by_name,
                source_dirs,
                ui,
            )?;
            let executable = stage_and_inspect(staged_root, participant, &source)?;
            let cwd = source_cwd(participant, resolved, source_dirs);
            specs.push(participant_spec(
                participant,
                &robot_key,
                kind,
                executable,
                cwd,
            )?);
        }
    }
    Ok(())
}

fn register_simulation_managed_participant(
    board: &BoardBackend,
    key: ProcessKey,
    status: ParticipantStatus,
    incarnation: u64,
    startup_requirement: StartupRequirement,
) {
    board.upsert_process(key.clone(), status, startup_requirement);
    // Webots-owned controllers are intentionally outside `ManagedChild`, so
    // their launch record keeps the reserved unmanaged incarnation (zero).
    // Record that exact value on the board: observed Liveliness remains the
    // readiness proof, but can now satisfy the same exact-incarnation gate as
    // a supervisor-minted child.
    board.set_incarnation(&key, incarnation);
}

/// The board `ParticipantKind` plus whether the participant runs from local
/// (user/robot-owned) code, for a participant's source-free `execution` (#936).
/// The role alone decides both: officials and tools are framework binaries
/// (`local = false`), user services and component drivers are the robot's own
/// code (`local = true`). An official service the robot happens to override in
/// its Cargo workspace still resolves to the one official `bin/` entry, so it
/// is reported the same as a vendored one - the plan no longer distinguishes
/// "overridden" from "vendored", because the layout it is built from cannot.
pub(crate) fn participant_kind(execution: &ParticipantExecution) -> (ParticipantKind, bool) {
    match execution {
        ParticipantExecution::OfficialArtifact { .. } => (ParticipantKind::Service, false),
        ParticipantExecution::OfficialTool { .. } => (ParticipantKind::Tool, false),
        ParticipantExecution::UserService { .. } => (ParticipantKind::Service, true),
        ParticipantExecution::ComponentDriver { .. } => (ParticipantKind::Driver, true),
    }
}

pub(crate) fn spec_from_launch_record(
    participant: &ParticipantLaunchRecord,
    resolved: &ResolvedRobot,
    source_dirs: &BTreeMap<String, PathBuf>,
    staged_root: &Path,
    ui: &crate::Ui,
) -> Result<Option<ParticipantSpec>> {
    if participant.launch_ownership == LaunchOwnership::SimulationManaged {
        return Ok(None);
    }
    let robot = RobotKey::new(&participant.launch.namespace, &participant.launch.robot_id);
    // `_local`: this function only builds a `ParticipantSpec` (no
    // `ParticipantStatus` to mark `.with_local` on) - see the other
    // `participant_kind` call sites for where the bool is actually consumed.
    let (kind, _local) = participant_kind(&participant.execution);
    let official_by_name = official_runtimes_by_name(resolved);
    let source =
        resolve_participant_source(participant, resolved, &official_by_name, source_dirs, ui)?;
    let executable = stage_and_inspect(staged_root, participant, &source)?;
    let cwd = source_cwd(participant, resolved, source_dirs);
    Ok(Some(participant_spec(
        participant,
        &robot,
        kind,
        executable,
        cwd,
    )?))
}

/// Every official platform runtime the loader may need to resolve, keyed by its
/// launch identity: the services and simulators in `platform_runtimes` plus the
/// suite-sourced component drivers carried on `components[].driver.suite_runtime`
/// (a suite driver projects onto the same `ResolvedPlatformRuntime` shape and is
/// keyed by its component id). Source-sourced drivers are not here - they build
/// from their crate through the source-execution path.
fn official_runtimes_by_name(resolved: &ResolvedRobot) -> BTreeMap<&str, &ResolvedPlatformRuntime> {
    resolved
        .platform_runtimes
        .iter()
        .chain(
            resolved
                .components
                .iter()
                .filter_map(|component| component.driver.as_ref())
                .filter_map(|driver| driver.suite_runtime.as_ref()),
        )
        .map(|runtime| (runtime.name.as_str(), runtime))
        .collect()
}

/// Resolve the binary one launched participant runs from, so it can be staged
/// into the layout's `bin/`. The source-free plan (#936) names only the
/// participant's role and `bin/` binary, so where its bytes come from is
/// recovered here from the resolved graph and the staging-side `source_dirs`
/// record: a user service and a workspace-built component driver build through
/// `cargo` from their crate directory; an official artifact, tool, or
/// suite-provided driver resolves from its own source override or the vendored
/// `.phoxal/artifacts` store. Every path hard-fails - there is no graceful
/// `None`/pending note - naming the required identity.
fn resolve_participant_source(
    participant: &ParticipantLaunchRecord,
    resolved: &ResolvedRobot,
    official_by_name: &BTreeMap<&str, &ResolvedPlatformRuntime>,
    source_dirs: &BTreeMap<String, PathBuf>,
    ui: &crate::Ui,
) -> Result<PathBuf> {
    let id = &participant.launch.participant_id;
    let mut build_override =
        |crate_dir: &Path, name: &str| build_source_binary(crate_dir, name, ui);
    match &participant.execution {
        ParticipantExecution::UserService { .. } => {
            let crate_dir = source_dirs.get(id).ok_or_else(|| {
                anyhow!("staged plan is missing the source crate directory for user service {id}")
            })?;
            build_source_binary(crate_dir, id, ui)
        }
        ParticipantExecution::ComponentDriver { .. } => {
            // A workspace-built driver has a crate directory in the staging
            // record; a suite-provided one does not and resolves from the
            // vendored store by its component id, exactly like a service.
            if let Some(crate_dir) = source_dirs.get(id) {
                return build_source_binary(crate_dir, id, ui);
            }
            let runtime = official_by_name
                .get(participant.artifact_id.as_str())
                .ok_or_else(|| {
                    anyhow!(
                        "resolved graph is missing component driver {}",
                        participant.artifact_id
                    )
                })?;
            crate::stager::resolve_platform_source(runtime, &mut build_override)
        }
        ParticipantExecution::OfficialTool { .. } => {
            let tool = resolved
                .tools
                .iter()
                .find(|tool| tool.name == participant.artifact_id)
                .ok_or_else(|| {
                    anyhow!("resolved graph is missing tool {}", participant.artifact_id)
                })?;
            crate::stager::resolve_tool_source(tool, &mut build_override)
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
            crate::stager::resolve_platform_source(runtime, &mut build_override)
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
    let staged = crate::stager::stage_participant_binary(staged_root, participant, source)?;
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
/// (overridden officials and tools).
fn source_cwd(
    participant: &ParticipantLaunchRecord,
    resolved: &ResolvedRobot,
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
        ParticipantExecution::OfficialTool { .. } => resolved
            .tools
            .iter()
            .find(|tool| tool.name == participant.artifact_id)
            .and_then(|tool| tool.path_override.clone()),
    }
}

/// Whether a participant launches with tool env (privileged) rather than the
/// bus-participant env: an official tool, vendored or overridden.
fn is_tool_execution(execution: &ParticipantExecution) -> bool {
    matches!(execution, ParticipantExecution::OfficialTool { .. })
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
    let env = if is_tool_execution(&participant.execution) {
        encode_tool_env(&participant.launch)?
    } else {
        encode_participant_env(&participant.launch)?
    };
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
    use phoxal::participant::launch::{
        BusProfile, ClockMode, DEFAULT_SHUTDOWN_GRACE_MS, ParticipantLaunch,
    };
    use phoxal_cli_core::check::participant_metadata::host_architecture;
    use phoxal_cli_core::session::RuntimeFailurePolicy;
    use phoxal_cli_core::session::{ParticipantInstanceKey, ParticipantKind};

    /// Synthesize an ELF of a given architecture carrying the phoxal metadata
    /// section, so `stage_and_inspect` is exercised against real object shapes
    /// without building a binary (mirrors the loader's own test synthesis).
    fn synthesize_binary(arch: object::Architecture) -> Vec<u8> {
        use object::write::Object;
        let mut obj = Object::new(object::BinaryFormat::Elf, arch, object::Endianness::Little);
        let section = obj.add_section(
            Vec::new(),
            b".phoxal_api_meta".to_vec(),
            object::SectionKind::ReadOnlyData,
        );
        let payload = br#"{"participant_api":"()","contracts":[],"config_schema":{"type":"null"}}"#;
        obj.append_section_data(section, payload, 1);
        obj.write().expect("synthesize object file")
    }

    fn user_service_record(id: &str) -> ParticipantLaunchRecord {
        ParticipantLaunchRecord {
            artifact_id: id.to_string(),
            execution: ParticipantExecution::UserService {
                binary_name: id.to_string(),
            },
            launch: ParticipantLaunch {
                participant_id: id.to_string(),
                incarnation: 0,
                namespace: "dev".to_string(),
                robot_id: "robot_v1".to_string(),
                bus: BusProfile {
                    connect_endpoints: Vec::new(),
                },
                clock: ClockMode::Real,
                config: None,
                robot_root: None,
                component_instance: None,
                execution_device_id: None,
                shutdown_grace_ms: DEFAULT_SHUTDOWN_GRACE_MS,
            },
            launch_ownership: LaunchOwnership::CliManaged,
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
    fn simulation_managed_readiness_accepts_its_exact_unmanaged_incarnation() {
        let board = BoardBackend::new();
        let robot = RobotKey::new("dev", "rover");
        let key = ProcessKey::robot(robot.clone(), "simulator-webots-supervisor");
        register_simulation_managed_participant(
            &board,
            key.clone(),
            ParticipantStatus::new(
                key.to_string(),
                ParticipantKind::Simulator,
                ParticipantState::Starting,
            ),
            0,
            StartupRequirement::Required,
        );

        board.record_instance_presence(
            ParticipantInstanceKey {
                robot,
                participant: "simulator-webots-supervisor".to_string(),
                incarnation: 0,
            },
            true,
        );

        assert_eq!(
            board.snapshot().participants[&key.to_string()].state,
            ParticipantState::Ready
        );
    }
}
