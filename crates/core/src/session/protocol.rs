//! Bounded, CLI-internal protocol for the project-local resident supervisor.

use serde::{Deserialize, Serialize};

use super::{IncarnationId, ProcessKey, ProcessScope, SupervisorSnapshotV0};

pub const SUPERVISOR_PROTOCOL_VERSION: u16 = 0;
pub const MAX_SUPERVISED_PROCESSES: usize = 40;
pub const MAX_PROCESS_FAILURE_DETAIL_BYTES: usize = 4 * 1024;
pub const MAX_PROCESS_STDERR_TAIL_BYTES: usize = 32 * 1024;
pub const MAX_ARTIFACT_ID_BYTES: usize = 1024;
pub const MAX_SNAPSHOT_TEXT_BYTES: usize = 4 * 1024;
pub const MAX_HANDSHAKE_FRAME_BYTES: usize = 4 * 1024;
pub const MAX_COMMAND_FRAME_BYTES: usize = 64 * 1024;
pub const MAX_SNAPSHOT_FRAME_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_RECENT_COMMAND_REPLIES: usize = 128;
pub const MAX_STARTUP_PHASES: usize = 64;

/// Conservative encoded upper bound for one maximum-size process entry.
///
/// JSON escaping can expand every byte to six bytes (`\u00xx`), so the
/// calculation deliberately assumes that worst case for every bounded text
/// field and includes structural headroom.
pub const MAX_ENCODED_PROCESS_BYTES: usize = 6
    * (3 * MAX_ARTIFACT_ID_BYTES
        + MAX_PROCESS_FAILURE_DETAIL_BYTES
        + MAX_PROCESS_STDERR_TAIL_BYTES
        + 5 * MAX_SNAPSHOT_TEXT_BYTES)
    + 8 * 1024;
pub const MAX_ENCODED_SNAPSHOT_FIXED_BYTES: usize =
    6 * (10 + MAX_STARTUP_PHASES + 1) * MAX_SNAPSHOT_TEXT_BYTES + 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionRole {
    Snapshots,
    Commands,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HandshakeRequest {
    pub protocol_version: u16,
    pub role: ConnectionRole,
    pub resume_command_session: Option<CommandSessionId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HandshakeReply {
    pub protocol_version: u16,
    pub supervisor_generation: u64,
    pub command_session: Option<CommandSessionId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct CommandSessionId(pub [u8; 16]);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandKey {
    pub session: CommandSessionId,
    pub sequence: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum CommandAction {
    Restart {
        process: ProcessKey,
        expected_incarnation: IncarnationId,
    },
    Shutdown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandRequest {
    pub supervisor_generation: u64,
    pub key: CommandKey,
    pub action: CommandAction,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandReply {
    pub accepted: bool,
    pub error: Option<CommandError>,
}

impl CommandReply {
    #[must_use]
    pub const fn accepted() -> Self {
        Self {
            accepted: true,
            error: None,
        }
    }

    #[must_use]
    pub const fn rejected(error: CommandError) -> Self {
        Self {
            accepted: false,
            error: Some(error),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandError {
    StaleSupervisorGeneration,
    InvalidSession,
    AlreadyProcessed,
    OutOfOrder,
    UnknownProcess,
    SupersededIncarnation,
    SupervisorUnavailable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaunchNonce(pub [u8; 32]);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum BootstrapResult {
    Bound {
        supervisor_generation: u64,
        launch_nonce: LaunchNonce,
    },
    Rejected {
        error: String,
    },
}

#[must_use]
pub const fn worst_case_snapshot_bytes(process_count: usize) -> Option<usize> {
    match MAX_ENCODED_PROCESS_BYTES.checked_mul(process_count) {
        Some(processes) => processes.checked_add(MAX_ENCODED_SNAPSHOT_FIXED_BYTES),
        None => None,
    }
}

pub fn validate_snapshot_capacity(process_count: usize) -> anyhow::Result<()> {
    anyhow::ensure!(
        process_count <= MAX_SUPERVISED_PROCESSES,
        "execution plan has {process_count} supervised processes; protocol v0 supports at most {MAX_SUPERVISED_PROCESSES}"
    );
    let bytes = worst_case_snapshot_bytes(process_count)
        .ok_or_else(|| anyhow::anyhow!("worst-case supervisor snapshot size overflow"))?;
    anyhow::ensure!(
        bytes <= MAX_SNAPSHOT_FRAME_BYTES,
        "execution plan worst-case supervisor snapshot is {bytes} bytes; protocol v0 frame ceiling is {MAX_SNAPSHOT_FRAME_BYTES} bytes"
    );
    Ok(())
}

pub fn validate_snapshot_bounds(snapshot: &SupervisorSnapshotV0) -> anyhow::Result<()> {
    validate_snapshot_capacity(snapshot.processes.len())?;
    for (name, value) in [
        ("project", snapshot.project.as_str()),
        ("entry", snapshot.entry.as_str()),
        ("framework_train", snapshot.framework_train.as_str()),
        ("execution", snapshot.execution.as_str()),
        ("router", snapshot.router.as_str()),
    ] {
        anyhow::ensure!(
            value.len() <= MAX_SNAPSHOT_TEXT_BYTES,
            "supervisor snapshot {name} is {} bytes; limit is {MAX_SNAPSHOT_TEXT_BYTES}",
            value.len()
        );
    }
    anyhow::ensure!(
        snapshot.startup.completed_phases.len() <= MAX_STARTUP_PHASES,
        "supervisor snapshot has {} completed startup phases; limit is {MAX_STARTUP_PHASES}",
        snapshot.startup.completed_phases.len()
    );
    for phase in snapshot
        .startup
        .completed_phases
        .iter()
        .chain(snapshot.startup.active_phase.iter())
    {
        anyhow::ensure!(
            phase.len() <= MAX_SNAPSHOT_TEXT_BYTES,
            "supervisor startup phase is {} bytes; limit is {MAX_SNAPSHOT_TEXT_BYTES}",
            phase.len()
        );
    }
    for (key, entry) in &snapshot.processes {
        validate_process_key(key)?;
        anyhow::ensure!(
            entry.descriptor.artifact.len() <= MAX_ARTIFACT_ID_BYTES,
            "process {key} artifact id is {} bytes; limit is {MAX_ARTIFACT_ID_BYTES}",
            entry.descriptor.artifact.len()
        );
        anyhow::ensure!(
            entry.descriptor.owner.len() <= MAX_SNAPSHOT_TEXT_BYTES,
            "process {key} owner is {} bytes; limit is {MAX_SNAPSHOT_TEXT_BYTES}",
            entry.descriptor.owner.len()
        );
        if let Some(failure) = &entry.status.last_failure {
            anyhow::ensure!(
                failure.detail.as_str().len() <= MAX_PROCESS_FAILURE_DETAIL_BYTES,
                "process {key} failure detail is {} bytes; limit is {MAX_PROCESS_FAILURE_DETAIL_BYTES}",
                failure.detail.as_str().len()
            );
            if let Some(stderr) = &failure.stderr_tail {
                anyhow::ensure!(
                    stderr.as_str().len() <= MAX_PROCESS_STDERR_TAIL_BYTES,
                    "process {key} stderr tail is {} bytes; limit is {MAX_PROCESS_STDERR_TAIL_BYTES}",
                    stderr.as_str().len()
                );
            }
        }
    }
    let encoded = serde_json::to_vec(snapshot)?;
    anyhow::ensure!(
        encoded.len() <= MAX_SNAPSHOT_FRAME_BYTES,
        "encoded supervisor snapshot is {} bytes; frame limit is {MAX_SNAPSHOT_FRAME_BYTES}",
        encoded.len()
    );
    Ok(())
}

fn validate_process_key(key: &ProcessKey) -> anyhow::Result<()> {
    anyhow::ensure!(
        key.id.len() <= MAX_ARTIFACT_ID_BYTES,
        "process id is {} bytes; limit is {MAX_ARTIFACT_ID_BYTES}",
        key.id.len()
    );
    if let ProcessScope::Robot(robot) = &key.scope {
        for (name, value) in [
            ("namespace", robot.namespace.as_str()),
            ("robot id", robot.robot_id.as_str()),
        ] {
            anyhow::ensure!(
                value.len() <= MAX_SNAPSHOT_TEXT_BYTES,
                "process {name} is {} bytes; limit is {MAX_SNAPSHOT_TEXT_BYTES}",
                value.len()
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::supervisor::{
        BoundedString, DesiredProcessState, ProcessDescriptor, ProcessEntry, ProcessFailure,
        ProcessFailureKind, ProcessState, ProcessStatus, StartupStatus,
    };
    use crate::session::{ParticipantKind, RobotKey, StartupRequirement};
    use std::collections::BTreeMap;
    use std::time::SystemTime;

    #[test]
    fn maximum_graph_fits_and_one_more_process_is_rejected() {
        assert_eq!(MAX_SUPERVISED_PROCESSES, 40);
        validate_snapshot_capacity(MAX_SUPERVISED_PROCESSES).unwrap();
        assert!(validate_snapshot_capacity(MAX_SUPERVISED_PROCESSES + 1).is_err());
        assert!(
            worst_case_snapshot_bytes(MAX_SUPERVISED_PROCESSES).unwrap()
                <= MAX_SNAPSHOT_FRAME_BYTES
        );
        assert!(
            worst_case_snapshot_bytes(MAX_SUPERVISED_PROCESSES + 1).unwrap()
                > MAX_SNAPSHOT_FRAME_BYTES
        );
    }

    #[test]
    fn maximum_terminal_graph_serializes_below_v0_frame_ceiling() {
        let mut processes = BTreeMap::new();
        for index in 0..MAX_SUPERVISED_PROCESSES {
            let key = ProcessKey::robot(
                RobotKey::new(
                    "n".repeat(MAX_SNAPSHOT_TEXT_BYTES),
                    "r".repeat(MAX_SNAPSHOT_TEXT_BYTES),
                ),
                format!("p{index:02}{}", "x".repeat(MAX_ARTIFACT_ID_BYTES - 3)),
            );
            processes.insert(
                key.clone(),
                ProcessEntry {
                    descriptor: ProcessDescriptor {
                        key,
                        kind: ParticipantKind::Service,
                        artifact: "a".repeat(MAX_ARTIFACT_ID_BYTES),
                        owner: "o".repeat(MAX_SNAPSHOT_TEXT_BYTES),
                        startup_requirement: StartupRequirement::Required,
                    },
                    status: ProcessStatus {
                        desired: DesiredProcessState::Running,
                        actual: ProcessState::Failed,
                        pid: Some(u32::MAX),
                        incarnation: Some(u64::MAX),
                        restart_count_in_generation: u32::MAX,
                        restart_count_total: u64::MAX,
                        last_failure: Some(ProcessFailure {
                            kind: ProcessFailureKind::Exit,
                            occurred_at: SystemTime::UNIX_EPOCH,
                            exit: None,
                            detail: BoundedString::with_max_bytes(
                                "d".repeat(MAX_PROCESS_FAILURE_DETAIL_BYTES),
                                MAX_PROCESS_FAILURE_DETAIL_BYTES,
                            ),
                            stderr_tail: Some(BoundedString::with_max_bytes(
                                "e".repeat(MAX_PROCESS_STDERR_TAIL_BYTES),
                                MAX_PROCESS_STDERR_TAIL_BYTES,
                            )),
                        }),
                    },
                },
            );
        }
        let snapshot = SupervisorSnapshotV0 {
            supervisor_generation: u64::MAX,
            revision: u64::MAX,
            project: "p".repeat(MAX_SNAPSHOT_TEXT_BYTES),
            entry: "e".repeat(MAX_SNAPSHOT_TEXT_BYTES),
            framework_train: "t".repeat(MAX_SNAPSHOT_TEXT_BYTES),
            execution: "x".repeat(MAX_SNAPSHOT_TEXT_BYTES),
            lifecycle: super::super::ProjectLifecycle::Failed,
            router: "r".repeat(MAX_SNAPSHOT_TEXT_BYTES),
            plan_revision: u64::MAX,
            graph_generation: u64::MAX,
            startup: StartupStatus {
                completed_phases: (0..MAX_STARTUP_PHASES)
                    .map(|_| "s".repeat(MAX_SNAPSHOT_TEXT_BYTES))
                    .collect(),
                active_phase: Some("s".repeat(MAX_SNAPSHOT_TEXT_BYTES)),
            },
            processes,
        };
        validate_snapshot_bounds(&snapshot).unwrap();
        assert!(serde_json::to_vec(&snapshot).unwrap().len() <= MAX_SNAPSHOT_FRAME_BYTES);

        let mut too_many = snapshot;
        let key = ProcessKey::project("overflow");
        too_many.processes.insert(
            key.clone(),
            ProcessEntry {
                descriptor: ProcessDescriptor {
                    key,
                    kind: ParticipantKind::Service,
                    artifact: "overflow".to_string(),
                    owner: "phoxal-cli".to_string(),
                    startup_requirement: StartupRequirement::Required,
                },
                status: ProcessStatus::default(),
            },
        );
        assert!(validate_snapshot_bounds(&too_many).is_err());
    }
}
