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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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

/// Parameters that turn a normalized manual input into a physical
/// differential-drive command, derived once by the resident from the robot's
/// authored kinematics and motion limits.
///
/// This travels on the snapshot because the pad is attached to whichever
/// machine runs the client, which may not be the machine that holds the robot
/// model (organization#978).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManualDrive {
    pub wheel_base_m: f64,
    /// The speed one differential side reaches at full trigger, already
    /// clamped by both the linear and angular authored limits.
    pub side_speed_mps: f64,
}

/// Whether this robot can be driven by manual input, and why not when it
/// cannot. `Unsupported` is a property of the robot model, not of the
/// operator's hardware - the client shows the reason rather than an empty
/// device list.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManualInput {
    Supported(ManualDrive),
    Unsupported(String),
}

impl ManualInput {
    /// The drive parameters, if this robot supports manual input.
    #[must_use]
    pub fn drive(&self) -> Option<ManualDrive> {
        match self {
            Self::Supported(drive) => Some(*drive),
            Self::Unsupported(_) => None,
        }
    }

    /// Why manual input is unavailable, if it is.
    #[must_use]
    pub fn unsupported_reason(&self) -> Option<&str> {
        match self {
            Self::Supported(_) => None,
            Self::Unsupported(reason) => Some(reason),
        }
    }
}

impl ManualDrive {
    /// Derive the parameters from a robot's authored wheel base and motion
    /// limits. This crate stays model-free on purpose, so the caller reads
    /// those out of the robot model.
    ///
    /// # Errors
    ///
    /// Returns a human-readable reason when the authored values rule manual
    /// input out - which the client shows instead of an empty device list.
    pub fn derive(
        wheel_base_m: f64,
        max_linear_speed_mps: f64,
        max_angular_speed_radps: f64,
    ) -> Result<Self, String> {
        if !(wheel_base_m.is_finite() && wheel_base_m > 0.0) {
            return Err("robot.kinematic.wheel_base_m must be finite and > 0".to_string());
        }
        Ok(Self {
            wheel_base_m,
            side_speed_mps: max_linear_speed_mps.min(max_angular_speed_radps * wheel_base_m / 2.0),
        })
    }
}

// `ManualDrive` carries floats, so this snapshot is `PartialEq` but not `Eq`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
    pub graph_generation: u64,
    pub startup: StartupStatus,
    pub processes: BTreeMap<ProcessKey, ProcessEntry>,
    /// Why `lifecycle` reached `Failed`, set only alongside that transition.
    /// Populated for a resident-level failure (preparation or supervision
    /// error with no single process to blame); a single process's own
    /// failure lives on that process's `ProcessFailure.detail` instead.
    pub failure: Option<String>,
    /// How to turn manual input into a drive command for this robot.
    pub manual_input: ManualInput,
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
            graph_generation: 0,
            startup: StartupStatus::default(),
            processes: BTreeMap::new(),
            failure: None,
            manual_input: ManualInput::Unsupported("no robot model is loaded".to_string()),
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

#[cfg(test)]
mod manual_input_tests {
    use super::*;

    #[test]
    fn the_side_speed_takes_whichever_authored_limit_binds_first() {
        // Angular-limited: 2.0 rad/s over a 0.3 m base allows only 0.3 m/s a
        // side, well under the 0.6 m/s linear limit.
        let angular_limited = ManualDrive::derive(0.3, 0.6, 2.0).expect("valid wheel base");
        assert_eq!(angular_limited.side_speed_mps, 0.3);

        // Linear-limited: a generous angular limit must not raise the side
        // speed past what the robot is allowed to travel.
        let linear_limited = ManualDrive::derive(0.3, 0.2, 10.0).expect("valid wheel base");
        assert_eq!(linear_limited.side_speed_mps, 0.2);
    }

    #[test]
    fn an_unusable_wheel_base_is_rejected_with_a_reason() {
        for wheel_base_m in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            let error = ManualDrive::derive(wheel_base_m, 0.6, 2.0)
                .expect_err("an unusable wheel base must not yield drive parameters");
            assert!(
                error.contains("wheel_base_m must be finite and > 0"),
                "{error}"
            );
        }
    }

    #[test]
    fn manual_input_reports_either_parameters_or_a_reason_never_both() {
        let supported = ManualInput::Supported(ManualDrive::derive(0.3, 0.6, 2.0).unwrap());
        assert!(supported.drive().is_some());
        assert!(supported.unsupported_reason().is_none());

        let unsupported = ManualInput::Unsupported("not differential".to_string());
        assert!(unsupported.drive().is_none());
        assert_eq!(unsupported.unsupported_reason(), Some("not differential"));
    }
}
