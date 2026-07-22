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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoundedString(String);

impl BoundedString {
    pub const MAX_CHARS: usize = 4_096;

    #[must_use]
    pub fn new(value: impl AsRef<str>) -> Self {
        let value = value.as_ref();
        let mut chars = value.chars();
        let mut bounded = chars.by_ref().take(Self::MAX_CHARS).collect::<String>();
        if chars.next().is_some() {
            bounded.push('…');
        }
        Self(bounded)
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
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
pub struct SupervisorSnapshotV0 {
    pub supervisor_generation: u64,
    pub revision: u64,
    pub project: String,
    pub framework_train: String,
    pub execution: String,
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
            framework_train: String::new(),
            execution: String::new(),
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
