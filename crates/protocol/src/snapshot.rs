//! Version-tagged supervisor snapshot schema.

use std::collections::BTreeMap;

use phoxal_cli_core::identity::ExecutionId;
use phoxal_cli_core::runtime::{
    ProcessEntry, ProcessKey, ProcessScope, ProjectLifecycle, SimulationSessionInfo, StartupStatus,
};
use serde::{Deserialize, Serialize};

use crate::limits::{
    MAX_ARTIFACT_ID_BYTES, MAX_PROCESS_FAILURE_DETAIL_BYTES, MAX_PROCESS_STDERR_TAIL_BYTES,
    MAX_SNAPSHOT_TEXT_BYTES, MAX_STARTUP_STEPS, MAX_STEP_DETAIL_BYTES,
    MAX_SUPERVISOR_FAILURE_REASON_BYTES, validate_snapshot_capacity,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "schema")]
pub enum SupervisorSnapshot {
    #[serde(rename = "phoxal/supervisor-snapshot/v0")]
    V0(SupervisorSnapshotV0),
}

impl SupervisorSnapshot {
    #[must_use]
    pub const fn as_v0(&self) -> &SupervisorSnapshotV0 {
        match self {
            Self::V0(snapshot) => snapshot,
        }
    }

    #[must_use]
    pub fn into_v0(self) -> SupervisorSnapshotV0 {
        match self {
            Self::V0(snapshot) => snapshot,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SupervisorSnapshotV0 {
    pub supervisor_generation: u64,
    pub revision: u64,
    pub execution_id: ExecutionId,
    pub project: String,
    pub entry: String,
    pub framework_train: String,
    pub simulation: Option<SimulationSessionInfo>,
    pub lifecycle: ProjectLifecycle,
    pub router: String,
    pub plan_revision: u64,
    pub graph_generation: u64,
    pub startup: StartupStatus,
    pub processes: BTreeMap<ProcessKey, ProcessEntry>,
    /// Why `lifecycle` reached `Failed`, set only alongside that transition.
    /// Populated for a resident-level failure (preparation or supervision
    /// error with no single process to blame); a single process's own
    /// failure lives on that process's `ProcessFailure.detail` instead.
    pub failure: Option<String>,
}

impl Default for SupervisorSnapshotV0 {
    fn default() -> Self {
        Self {
            supervisor_generation: 0,
            revision: 0,
            execution_id: ExecutionId::mint(),
            project: String::new(),
            entry: String::new(),
            framework_train: String::new(),
            simulation: None,
            lifecycle: ProjectLifecycle::Starting,
            router: String::new(),
            plan_revision: 0,
            graph_generation: 0,
            startup: StartupStatus::default(),
            processes: BTreeMap::new(),
            failure: None,
        }
    }
}

pub fn validate_snapshot_bounds(snapshot: &SupervisorSnapshotV0) -> anyhow::Result<()> {
    validate_snapshot_capacity(snapshot.processes.len())?;
    for (name, value) in [
        ("project", snapshot.project.as_str()),
        ("entry", snapshot.entry.as_str()),
        ("framework_train", snapshot.framework_train.as_str()),
        ("router", snapshot.router.as_str()),
    ] {
        anyhow::ensure!(
            value.len() <= MAX_SNAPSHOT_TEXT_BYTES,
            "supervisor snapshot {name} is {} bytes; limit is {MAX_SNAPSHOT_TEXT_BYTES}",
            value.len()
        );
    }
    if let Some(failure) = &snapshot.failure {
        anyhow::ensure!(
            failure.len() <= MAX_SUPERVISOR_FAILURE_REASON_BYTES,
            "supervisor snapshot failure reason is {} bytes; limit is {MAX_SUPERVISOR_FAILURE_REASON_BYTES}",
            failure.len()
        );
    }
    if let Some(simulation) = &snapshot.simulation {
        for (name, value) in [
            ("simulation.profile", simulation.profile.as_str()),
            ("simulation.world", simulation.world.as_str()),
        ] {
            anyhow::ensure!(
                value.len() <= MAX_SNAPSHOT_TEXT_BYTES,
                "supervisor snapshot {name} is {} bytes; limit is {MAX_SNAPSHOT_TEXT_BYTES}",
                value.len()
            );
        }
    }
    anyhow::ensure!(
        snapshot.startup.steps.len() <= MAX_STARTUP_STEPS,
        "supervisor snapshot has {} startup steps; limit is {MAX_STARTUP_STEPS}",
        snapshot.startup.steps.len()
    );
    for step in &snapshot.startup.steps {
        if let Some(detail) = &step.detail {
            anyhow::ensure!(
                detail.len() <= MAX_STEP_DETAIL_BYTES,
                "supervisor startup step detail is {} bytes; limit is {MAX_STEP_DETAIL_BYTES}",
                detail.len()
            );
        }
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
