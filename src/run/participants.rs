//! Participants responsibilities for run.

use super::{
    DriverPolicy, build_source_binary, device_missing_note, env_path_override,
    native_pending_official_note, native_pending_tool_note,
};
use crate::supervisor::BoardBackend;
use crate::supervisor::ParticipantSpec;
use crate::supervisor::ParticipantState;
use crate::supervisor::ParticipantStatus;
use anyhow::Result;
use anyhow::anyhow;
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

pub(crate) fn prepare_robot_participants(
    plan: &LaunchPlan,
    resolved: &ResolvedRobot,
    _project_root: &Path,
    driver_policy: &DriverPolicy,
    board: &BoardBackend,
    specs: &mut Vec<ParticipantSpec>,
    ui: &crate::Ui,
) -> Result<()> {
    let official_by_name = resolved
        .platform_runtimes
        .iter()
        .map(|runtime| (runtime.name.as_str(), runtime))
        .collect::<BTreeMap<_, _>>();
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
            match &participant.execution {
                ParticipantExecution::OfficialTool { .. } => {
                    match locate_tool_binary(resolved, &participant.artifact_id, ui)? {
                        Some(path) => specs.push(ParticipantSpec {
                            key: key.clone(),
                            id,
                            kind,
                            executable: path,
                            args: Vec::new(),
                            cwd: None,
                            env: encode_tool_env(&participant.launch)?.spawn_env(),
                            shutdown_grace: Duration::from_millis(
                                participant.launch.shutdown_grace_ms,
                            ),
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
                        }),
                        None => board.set_state(
                            &key,
                            ParticipantState::Failed,
                            Some(native_pending_tool_note(&participant.artifact_id)),
                        ),
                    }
                }
                ParticipantExecution::OfficialArtifact { .. } => {
                    let runtime = official_by_name
                        .get(participant.artifact_id.as_str())
                        .copied();
                    match locate_official_binary(runtime, &participant.artifact_id)? {
                        Some(path) => specs.push(ParticipantSpec {
                            key: key.clone(),
                            id,
                            kind,
                            executable: path,
                            args: Vec::new(),
                            cwd: None,
                            env: encode_participant_env(&participant.launch)?.spawn_env(),
                            shutdown_grace: Duration::from_millis(
                                participant.launch.shutdown_grace_ms,
                            ),
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
                        }),
                        None => board.set_state(
                            &key,
                            ParticipantState::Failed,
                            Some(native_pending_official_note(
                                runtime,
                                &participant.artifact_id,
                            )),
                        ),
                    }
                }
                ParticipantExecution::UserService { crate_dir } => {
                    let binary = build_source_binary(crate_dir, &id, ui)?;
                    specs.push(ParticipantSpec {
                        key: key.clone(),
                        id,
                        kind,
                        executable: binary,
                        args: Vec::new(),
                        cwd: Some(crate_dir.clone()),
                        env: encode_participant_env(&participant.launch)?.spawn_env(),
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
                    });
                }
                ParticipantExecution::SourceArtifact {
                    kind: artifact_kind,
                    crate_dir,
                } => {
                    let binary = build_source_binary(crate_dir, &id, ui)?;
                    let env = if artifact_kind == "tool" {
                        encode_tool_env(&participant.launch)?
                    } else {
                        encode_participant_env(&participant.launch)?
                    };
                    specs.push(ParticipantSpec {
                        key: key.clone(),
                        id,
                        kind,
                        executable: binary,
                        args: Vec::new(),
                        cwd: Some(crate_dir.clone()),
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
                    });
                }
                ParticipantExecution::ComponentDriver { crate_dir } => {
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
                    let binary = build_source_binary(crate_dir, &id, ui)?;
                    specs.push(ParticipantSpec {
                        key: key.clone(),
                        id,
                        kind,
                        executable: binary,
                        args: Vec::new(),
                        cwd: Some(crate_dir.clone()),
                        env: encode_participant_env(&participant.launch)?.spawn_env(),
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
                    });
                }
            }
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

/// The board `ParticipantKind` plus whether the participant runs from a
/// locally resolved directory, for a checked participant's `execution`.
/// `SourceArtifact`'s own `kind: String` (`"tool"`/`"simulator"`/`"service"`,
/// set by `launch_plan::participant_execution` from
/// `check::SourceParticipantKind::shared_kind`) recovers the real role for a
/// locally source-overridden official artifact - a Run-mode launch plan only
/// ever contains Service and Driver participants (`Tool` and `Simulator`
/// checked participants are excluded upstream by
/// `launch_plan::is_robot_launch_participant`), so `"service"` is the only
/// value seen here in practice, but Sim-mode plans reuse this same helper
/// through `watch`, where a source-overridden simulator is possible.
pub(crate) fn participant_kind(execution: &ParticipantExecution) -> (ParticipantKind, bool) {
    match execution {
        ParticipantExecution::OfficialArtifact { .. } => (ParticipantKind::Service, false),
        ParticipantExecution::OfficialTool { .. } => (ParticipantKind::Tool, false),
        ParticipantExecution::UserService { .. } => (ParticipantKind::Service, true),
        ParticipantExecution::SourceArtifact { kind, .. } => {
            let kind = match kind.as_str() {
                "tool" => ParticipantKind::Tool,
                "simulator" => ParticipantKind::Simulator,
                _ => ParticipantKind::Service,
            };
            (kind, true)
        }
        ParticipantExecution::ComponentDriver { .. } => (ParticipantKind::Driver, true),
    }
}

pub(crate) fn spec_from_launch_record(
    participant: &ParticipantLaunchRecord,
    resolved: &ResolvedRobot,
    ui: &crate::Ui,
) -> Result<Option<ParticipantSpec>> {
    if participant.launch_ownership == LaunchOwnership::SimulationManaged {
        return Ok(None);
    }
    let id = participant.launch.participant_id.clone();
    let robot = RobotKey::new(&participant.launch.namespace, &participant.launch.robot_id);
    // `_local`: this function only builds a `ParticipantSpec` (no
    // `ParticipantStatus` to mark `.with_local` on) - see the other
    // `participant_kind` call sites for where the bool is actually consumed.
    let (kind, _local) = participant_kind(&participant.execution);
    let is_tool = match &participant.execution {
        ParticipantExecution::OfficialTool { .. } => true,
        ParticipantExecution::SourceArtifact { kind, .. } => kind == "tool",
        _ => false,
    };
    let (binary, cwd) = match &participant.execution {
        ParticipantExecution::UserService { crate_dir }
        | ParticipantExecution::SourceArtifact { crate_dir, .. }
        | ParticipantExecution::ComponentDriver { crate_dir } => (
            build_source_binary(crate_dir, &id, ui)?,
            Some(crate_dir.clone()),
        ),
        ParticipantExecution::OfficialTool { .. } => (
            locate_tool_binary(resolved, &participant.artifact_id, ui)?
                .ok_or_else(|| anyhow!(native_pending_tool_note(&participant.artifact_id)))?,
            None,
        ),
        ParticipantExecution::OfficialArtifact { .. } => {
            let runtime = resolved
                .platform_runtimes
                .iter()
                .find(|runtime| runtime.name == participant.artifact_id);
            (
                locate_official_binary(runtime, &participant.artifact_id)?.ok_or_else(|| {
                    anyhow!(native_pending_official_note(
                        runtime,
                        &participant.artifact_id
                    ))
                })?,
                None,
            )
        }
    };
    let env = if is_tool {
        encode_tool_env(&participant.launch)?
    } else {
        encode_participant_env(&participant.launch)?
    };
    Ok(Some(ParticipantSpec {
        key: ProcessKey::robot(robot.clone(), &id),
        id,
        kind,
        executable: binary,
        args: Vec::new(),
        cwd,
        env: env.spawn_env(),
        shutdown_grace: Duration::from_millis(participant.launch.shutdown_grace_ms),
        process_group: true,
        note: None,
        bus_participant: true,
        readiness: ParticipantSpec::exact_liveliness_template(
            robot,
            &participant.launch.participant_id,
        ),
        startup_requirement: participant.startup_requirement,
        runtime_failure: participant.runtime_failure,
        restart_policy: Default::default(),
    }))
}

pub(crate) fn locate_tool_binary(
    resolved: &ResolvedRobot,
    name: &str,
    ui: &crate::Ui,
) -> Result<Option<PathBuf>> {
    let tool = resolved
        .tools
        .iter()
        .find(|tool| tool.name == name)
        .ok_or_else(|| anyhow!("resolved graph is missing tool {name}"))?;
    if let Some(path) = &tool.path_override {
        return Ok(Some(build_source_binary(path, name, ui)?));
    }
    if let Some(path) = env_path_override("PHOXAL_ARTIFACT", name) {
        return Ok(Some(path));
    }
    if let Some(path) = env_path_override("PHOXAL_TOOL", name) {
        return Ok(Some(path));
    }
    if let Ok(dir) = std::env::var("PHOXAL_ARTIFACT_DIR") {
        let path = PathBuf::from(dir).join(&tool.binary_name);
        if path.is_file() {
            return Ok(Some(path));
        }
    }
    if let Ok(dir) = std::env::var("PHOXAL_TOOL_DIR") {
        for name in [&tool.name, &tool.binary_name] {
            let path = PathBuf::from(&dir).join(name);
            if path.is_file() {
                return Ok(Some(path));
            }
        }
    }
    let Some(descriptor) = phoxal_cli_core::artifacts::NativeArtifactDescriptor::from_tool(tool)?
    else {
        return Ok(None);
    };
    // Vendored-store resolution lives in the stager and never touches the
    // network; a store miss falls through to `None` so the caller reports the
    // "run `phoxal update`" board note rather than hard-failing the run.
    Ok(crate::stager::resolve_vendored_binary(&descriptor).ok())
}

pub(crate) fn locate_official_binary(
    runtime: Option<&ResolvedPlatformRuntime>,
    participant_id: &str,
) -> Result<Option<PathBuf>> {
    if let Some(path) = env_path_override("PHOXAL_ARTIFACT", participant_id) {
        return Ok(Some(path));
    }
    let binary_name = runtime
        .map(|runtime| {
            phoxal_cli_core::project::resolver::official_binary_name(runtime.kind, &runtime.name)
        })
        .unwrap_or_else(|| participant_id.to_string());
    if let Ok(dir) = std::env::var("PHOXAL_ARTIFACT_DIR") {
        let path = PathBuf::from(dir).join(&binary_name);
        if path.is_file() {
            return Ok(Some(path));
        }
    }
    if let Some(runtime) = runtime
        && let Some(descriptor) =
            phoxal_cli_core::artifacts::NativeArtifactDescriptor::from_runtime(runtime)?
    {
        // Vendored-store resolution lives in the stager and never touches the
        // network; a store miss falls through to `None` so the caller reports
        // the "run `phoxal update`" board note rather than hard-failing.
        return Ok(crate::stager::resolve_vendored_binary(&descriptor).ok());
    }
    // No env override, and no resolved runtime to derive a native-artifact
    // descriptor from (a path-overridden or otherwise non-suite runtime) -
    // the project-local store has no other identity from which to find this
    // participant's binary.
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use phoxal_cli_core::session::{ParticipantInstanceKey, ParticipantKind};

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
