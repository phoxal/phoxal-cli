//! Supervisor process state keyed by persisted participant identity.

use std::fmt;
use std::time::SystemTime;

use phoxal_runtime_contract::identity::ParticipantId;

use phoxal_runtime_contract::identity::ProducerId;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct ProcessKey(ParticipantId);

impl ProcessKey {
    #[must_use]
    pub(crate) const fn participant(&self) -> &ParticipantId {
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProcessState {
    Starting,
    Ready,
    Restarting,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProcessFailureKind {
    Spawn,
    Exit,
    ReadinessTimeout,
    ReadinessConflict,
    Cleanup,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ExitDescription {
    pub(crate) code: Option<i32>,
    pub(crate) signal: Option<i32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BoundedString(String);

impl BoundedString {
    pub(crate) const FAILURE_MAX_BYTES: usize = 4 * 1024;

    #[must_use]
    pub(crate) fn new(value: impl AsRef<str>) -> Self {
        Self::with_max_bytes(value, Self::FAILURE_MAX_BYTES)
    }

    #[must_use]
    pub(crate) fn with_max_bytes(value: impl AsRef<str>, maximum: usize) -> Self {
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
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProcessFailure {
    pub(crate) kind: ProcessFailureKind,
    pub(crate) occurred_at: SystemTime,
    pub(crate) exit: Option<ExitDescription>,
    pub(crate) detail: BoundedString,
    pub(crate) stderr_tail: Option<BoundedString>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProcessStatus {
    pub(crate) actual: ProcessState,
    pub(crate) pid: Option<u32>,
    pub(crate) producer: Option<ProducerId>,
    pub(crate) restart_count_total: u64,
    pub(crate) last_failure: Option<ProcessFailure>,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProcessEntry {
    pub(crate) status: ProcessStatus,
}
