//! Participants responsibilities for run.

use super::{
    DriverPolicy, build_source_binary, device_missing_note, env_path_override,
    native_pending_official_note, native_pending_tool_note,
};
use crate::supervisor::BoardBackend;
use crate::supervisor::ParticipantSpec;
use crate::supervisor::ParticipantState;
use crate::supervisor::ParticipantStatus;
use crate::supervisor::default_connect_endpoint;
use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use phoxal::participant::launch::env;
use phoxal_cli_core::project::launch_plan::LaunchOwnership;
use phoxal_cli_core::project::launch_plan::LaunchPlan;
use phoxal_cli_core::project::launch_plan::ParticipantExecution;
use phoxal_cli_core::project::launch_plan::ParticipantLaunchRecord;
use phoxal_cli_core::project::launch_plan::SITE_TOOL_JOYPAD;
use phoxal_cli_core::project::launch_plan::SiteLaunch;
use phoxal_cli_core::project::resolver::ResolvedPlatformRuntime;
use phoxal_cli_core::project::resolver::ResolvedRobot;
use phoxal_cli_core::session::ParticipantKind;
use phoxal_cli_core::session::launch_env::{encode_participant_env, encode_tool_env};
use phoxal_cli_core::session::stores::telemetry::RobotScope;
use phoxal_cli_core::session::{ProcessKey, RobotKey, RuntimeFailurePolicy, StartupRequirement};
use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DriverDecision {
    Launch,
    Degraded(String),
}

pub(crate) fn prepare_site_tools(
    plan: &LaunchPlan,
    resolved: &ResolvedRobot,
    robot_root: &Path,
    board: &BoardBackend,
    specs: &mut Vec<ParticipantSpec>,
    ui: &crate::Ui,
) -> Result<()> {
    let namespace = plan
        .robots
        .first()
        .map(|robot| robot.namespace.as_str())
        .unwrap_or("site");
    let robot_id = plan
        .robots
        .first()
        .map(|robot| robot.id.as_str())
        .unwrap_or("site");

    for site in &plan.site {
        let robot = RobotKey::new(namespace, robot_id);
        let key = ProcessKey::project(&site.id);
        let status =
            ParticipantStatus::new(&site.id, ParticipantKind::Tool, ParticipantState::Starting)
                .with_local(site_tool_is_local(resolved, &site.id))
                .with_scope(RobotScope {
                    namespace: namespace.to_string(),
                    robot_id: robot_id.to_string(),
                });
        board.upsert_process(key.clone(), status, StartupRequirement::Optional);
        match locate_tool_binary(resolved, &site.id, ui)? {
            Some(path) => specs.push(ParticipantSpec {
                key: key.clone(),
                id: site.id.clone(),
                kind: ParticipantKind::Tool,
                executable: path,
                args: Vec::new(),
                cwd: None,
                env: site_env(site, namespace, robot_id, robot_root)?,
                shutdown_grace: Duration::from_secs(5),
                process_group: true,
                note: None,
                bus_participant: true,
                readiness: ParticipantSpec::exact_liveliness_template(robot, &site.id),
                startup_requirement: StartupRequirement::Optional,
                runtime_failure: RuntimeFailurePolicy::KeepProjectDegraded,
                restart_policy: Default::default(),
            }),
            None => board.set_state(
                &key,
                ParticipantState::Failed,
                Some(native_pending_tool_note(&site.id)),
            ),
        }
    }
    Ok(())
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
                );
                continue;
            }
            board.upsert_process(
                key.clone(),
                ParticipantStatus::new(&id, kind, ParticipantState::Starting)
                    .with_local(local)
                    .with_scope(scope.clone()),
                StartupRequirement::Required,
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
                            startup_requirement: StartupRequirement::Required,
                            runtime_failure: RuntimeFailurePolicy::StopProject,
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
                            startup_requirement: StartupRequirement::Required,
                            runtime_failure: RuntimeFailurePolicy::StopProject,
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
                        startup_requirement: StartupRequirement::Required,
                        runtime_failure: RuntimeFailurePolicy::StopProject,
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
                        startup_requirement: StartupRequirement::Required,
                        runtime_failure: RuntimeFailurePolicy::StopProject,
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
                        startup_requirement: StartupRequirement::Required,
                        runtime_failure: RuntimeFailurePolicy::StopProject,
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
) {
    board.upsert_process(key.clone(), status, StartupRequirement::Required);
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
/// value seen here in practice, but Sim-mode plans reuse this same helper via
/// `source_spec_from_launch_record` (through `watch`), where a
/// source-overridden simulator is possible.
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

pub(crate) fn source_spec_from_launch_record(
    participant: &ParticipantLaunchRecord,
    ui: &crate::Ui,
) -> Result<Option<ParticipantSpec>> {
    let id = participant.launch.participant_id.clone();
    let robot = RobotKey::new(&participant.launch.namespace, &participant.launch.robot_id);
    // `_local`: this function only builds a `ParticipantSpec` (no
    // `ParticipantStatus` to mark `.with_local` on) - see the other
    // `participant_kind` call sites for where the bool is actually consumed.
    let (kind, _local) = participant_kind(&participant.execution);
    let is_tool = matches!(
        &participant.execution,
        ParticipantExecution::SourceArtifact { kind, .. } if kind == "tool"
    );
    let crate_dir = match &participant.execution {
        ParticipantExecution::UserService { crate_dir }
        | ParticipantExecution::SourceArtifact { crate_dir, .. }
        | ParticipantExecution::ComponentDriver { crate_dir } => crate_dir,
        ParticipantExecution::OfficialArtifact { .. }
        | ParticipantExecution::OfficialTool { .. } => return Ok(None),
    };
    let binary = build_source_binary(crate_dir, &id, ui)?;
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
        cwd: Some(crate_dir.clone()),
        env: env.spawn_env(),
        shutdown_grace: Duration::from_millis(participant.launch.shutdown_grace_ms),
        process_group: true,
        note: None,
        bus_participant: true,
        readiness: ParticipantSpec::exact_liveliness_template(
            robot,
            &participant.launch.participant_id,
        ),
        startup_requirement: StartupRequirement::Required,
        runtime_failure: RuntimeFailurePolicy::StopProject,
        restart_policy: Default::default(),
    }))
}

pub(crate) fn site_env(
    site: &SiteLaunch,
    namespace: &str,
    robot_id: &str,
    robot_root: &Path,
) -> Result<Vec<(String, String)>> {
    let mut envs = vec![
        (env::PARTICIPANT_ID.to_string(), site.id.clone()),
        (env::NAMESPACE.to_string(), namespace.to_string()),
        (env::ROBOT_ID.to_string(), robot_id.to_string()),
    ];
    if site.id == SITE_TOOL_JOYPAD {
        envs.push((
            env::ROBOT_ROOT.to_string(),
            robot_root.display().to_string(),
        ));
    }
    // A configless tool (`phoxal_config == Value::Null`)
    // must run with `PHOXAL_CONFIG` ABSENT: a unit config (`type Config = ()`)
    // fails to deserialize `{}` ("invalid type: map, expected unit"), and an
    // absent var uses the runner's null/unit fallback.
    if !site.phoxal_config.is_null() {
        envs.push((
            env::CONFIG.to_string(),
            serde_json::to_string(&site.phoxal_config)
                .with_context(|| format!("failed to encode PHOXAL_CONFIG for {}", site.id))?,
        ));
    }
    envs.push((env::CONNECT.to_string(), default_connect_endpoint()));
    Ok(envs)
}

/// Whether a tool is resolved from a local
/// path-pin override rather than a fetched suite artifact. Best-effort:
/// `false` if the tool is missing from `resolved.tools` (surfaced properly by
/// `locate_tool_binary`'s own lookup instead).
pub(crate) fn site_tool_is_local(resolved: &ResolvedRobot, name: &str) -> bool {
    resolved
        .tools
        .iter()
        .find(|tool| tool.name == name)
        .is_some_and(|tool| tool.path_override.is_some())
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
    let cache = crate::native_artifacts::artifact_binary_path(&descriptor)?;
    Ok(cache.is_file().then_some(cache))
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
        let binary = crate::native_artifacts::artifact_binary_path(&descriptor)?;
        return Ok(binary.is_file().then_some(binary));
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
