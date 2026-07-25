//! Process-only supervisor identity and revisioned state.
//!
//! This model deliberately excludes robot-bus presence, retained logs,
//! telemetry, and terminal presentation. Those remain independent client-side
//! authorities; the supervisor consumes exact Liveliness only while proving a
//! newly spawned process incarnation ready.

use std::collections::BTreeMap;
use std::fmt;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use super::ParticipantKind;

pub type IncarnationId = u64;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct RobotKey {
    pub namespace: String,
    pub robot_id: String,
}

impl RobotKey {
    #[must_use]
    pub fn new(namespace: impl Into<String>, robot_id: impl Into<String>) -> Self {
        Self {
            namespace: namespace.into(),
            robot_id: robot_id.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", content = "robot", rename_all = "snake_case")]
pub enum ProcessScope {
    Project,
    Robot(RobotKey),
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProcessKey {
    pub scope: ProcessScope,
    pub id: String,
}

impl ProcessKey {
    #[must_use]
    pub fn project(id: impl Into<String>) -> Self {
        Self {
            scope: ProcessScope::Project,
            id: id.into(),
        }
    }

    #[must_use]
    pub fn robot(robot: RobotKey, id: impl Into<String>) -> Self {
        Self {
            scope: ProcessScope::Robot(robot),
            id: id.into(),
        }
    }
}

impl fmt::Display for ProcessKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.scope {
            ProcessScope::Project => formatter.write_str(&self.id),
            ProcessScope::Robot(robot) => write!(
                formatter,
                "{}/{}::{}",
                robot.namespace, robot.robot_id, self.id
            ),
        }
    }
}

impl std::str::FromStr for ProcessKey {
    type Err = &'static str;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let Some((scope, id)) = value.rsplit_once("::") else {
            return Ok(Self::project(value));
        };
        let Some((namespace, robot_id)) = scope.rsplit_once('/') else {
            return Err("robot-scoped process key must contain namespace/robot");
        };
        if namespace.is_empty() || robot_id.is_empty() || id.is_empty() {
            return Err("process-key components cannot be empty");
        }
        Ok(Self::robot(RobotKey::new(namespace, robot_id), id))
    }
}

impl From<&str> for ProcessKey {
    fn from(value: &str) -> Self {
        value.parse().unwrap_or_else(|_| Self::project(value))
    }
}

impl From<String> for ProcessKey {
    fn from(value: String) -> Self {
        value.as_str().into()
    }
}

impl From<&ProcessKey> for ProcessKey {
    fn from(value: &ProcessKey) -> Self {
        value.clone()
    }
}

impl Serialize for ProcessKey {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for ProcessKey {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        value.parse().map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ParticipantInstanceKey {
    pub robot: RobotKey,
    pub participant: String,
    pub incarnation: IncarnationId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StartupRequirement {
    Required,
    Optional,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeFailurePolicy {
    KeepProjectDegraded,
    StopProject,
    RecreateGraph,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "instance", rename_all = "snake_case")]
pub enum ReadinessPolicy {
    ProcessSpawned,
    EndpointReady,
    ExactLiveliness(ParticipantInstanceKey),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectLifecycle {
    Starting,
    Ready,
    Degraded,
    Failed,
    Stopping,
    Stopped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DesiredProcessState {
    Running,
    Stopped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessState {
    Starting,
    Ready,
    Degraded,
    Restarting,
    Failed,
    Stopped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessFailureKind {
    Spawn,
    Exit,
    ReadinessTimeout,
    Cleanup,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExitDescription {
    pub code: Option<i32>,
    pub signal: Option<i32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BoundedString(String);

impl BoundedString {
    pub const MAX_BYTES: usize = super::protocol::MAX_PROCESS_STDERR_TAIL_BYTES;

    #[must_use]
    pub fn new(value: impl AsRef<str>) -> Self {
        Self::with_max_bytes(value, super::protocol::MAX_PROCESS_FAILURE_DETAIL_BYTES)
    }

    #[must_use]
    pub fn with_max_bytes(value: impl AsRef<str>, maximum: usize) -> Self {
        let value = value.as_ref();
        if value.len() <= maximum {
            return Self(value.to_string());
        }
        let suffix = "…";
        let mut end = maximum.saturating_sub(suffix.len()).min(value.len());
        while !value.is_char_boundary(end) {
            end = end.saturating_sub(1);
        }
        let mut bounded = value[..end].to_string();
        if maximum >= suffix.len() {
            bounded.push_str(suffix);
        }
        Self(bounded)
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for BoundedString {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        if value.len() > Self::MAX_BYTES {
            return Err(serde::de::Error::custom(format!(
                "bounded supervisor string is {} bytes; limit is {}",
                value.len(),
                Self::MAX_BYTES
            )));
        }
        Ok(Self(value))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessFailure {
    pub kind: ProcessFailureKind,
    pub occurred_at: SystemTime,
    pub exit: Option<ExitDescription>,
    pub detail: BoundedString,
    pub stderr_tail: Option<BoundedString>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessDescriptor {
    pub key: ProcessKey,
    pub kind: ParticipantKind,
    pub artifact: String,
    pub owner: String,
    pub startup_requirement: StartupRequirement,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessStatus {
    pub desired: DesiredProcessState,
    pub actual: ProcessState,
    pub pid: Option<u32>,
    pub incarnation: Option<IncarnationId>,
    pub restart_count_in_generation: u32,
    pub restart_count_total: u64,
    pub last_failure: Option<ProcessFailure>,
}

impl Default for ProcessStatus {
    fn default() -> Self {
        Self {
            desired: DesiredProcessState::Running,
            actual: ProcessState::Starting,
            pid: None,
            incarnation: None,
            restart_count_in_generation: 0,
            restart_count_total: 0,
            last_failure: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessEntry {
    pub descriptor: ProcessDescriptor,
    pub status: ProcessStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StartupStatus {
    pub completed_phases: Vec<String>,
    pub active_phase: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SimulationSessionInfo {
    pub profile: String,
    pub world: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SupervisorSnapshotV0 {
    pub supervisor_generation: u64,
    pub revision: u64,
    pub project: String,
    #[serde(default)]
    pub entry: String,
    pub framework_train: String,
    pub execution: String,
    #[serde(default)]
    pub simulation: Option<SimulationSessionInfo>,
    pub lifecycle: ProjectLifecycle,
    pub router: String,
    pub plan_revision: u64,
    pub graph_generation: u64,
    pub startup: StartupStatus,
    pub processes: BTreeMap<ProcessKey, ProcessEntry>,
}

impl Default for SupervisorSnapshotV0 {
    fn default() -> Self {
        Self {
            supervisor_generation: 0,
            revision: 0,
            project: String::new(),
            entry: String::new(),
            framework_train: String::new(),
            execution: String::new(),
            simulation: None,
            lifecycle: ProjectLifecycle::Starting,
            router: String::new(),
            plan_revision: 0,
            graph_generation: 0,
            startup: StartupStatus {
                completed_phases: Vec::new(),
                active_phase: None,
            },
            processes: BTreeMap::new(),
        }
    }
}
