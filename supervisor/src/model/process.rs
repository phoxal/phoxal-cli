//! Supervisor process state keyed by persisted participant identity.

use std::fmt;
use std::time::SystemTime;

use phoxal_runtime_contract::identity::ParticipantId;
use serde::{Deserialize, Serialize};

use super::{ParticipantKind, StartupRequirement};
use phoxal_runtime_contract::identity::ProducerId;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProcessKey(ParticipantId);

impl ProcessKey {
    #[must_use]
    pub const fn participant(&self) -> &ParticipantId {
        &self.0
    }
}

impl From<ParticipantId> for ProcessKey {
    fn from(value: ParticipantId) -> Self {
        Self(value)
    }
}

impl From<&ProcessKey> for ProcessKey {
    fn from(value: &ProcessKey) -> Self {
        value.clone()
    }
}

impl fmt::Display for ProcessKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
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
    ReadinessConflict,
    Cleanup,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExitDescription {
    pub code: Option<i32>,
    pub signal: Option<i32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BoundedString(String);

impl BoundedString {
    pub const MAX_BYTES: usize = 32 * 1024;
    pub const FAILURE_MAX_BYTES: usize = 4 * 1024;

    #[must_use]
    pub fn new(value: impl AsRef<str>) -> Self {
        Self::with_max_bytes(value, Self::FAILURE_MAX_BYTES)
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
    pub actual: ProcessState,
    pub pid: Option<u32>,
    pub producer: Option<ProducerId>,
    pub restart_count_total: u64,
    pub last_failure: Option<ProcessFailure>,
}

impl Default for ProcessStatus {
    fn default() -> Self {
        Self {
            actual: ProcessState::Starting,
            pid: None,
            producer: None,
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
