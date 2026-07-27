//! Persisted participant-board records and their terminal-neutral text form.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::ParticipantKind;
use super::stores::telemetry::RobotScope;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParticipantState {
    Starting,
    Ready,
    Degraded,
    Failed,
    Restarting,
    Stopped,
}

impl ParticipantState {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Ready => "ready",
            Self::Degraded => "degraded",
            Self::Failed => "failed",
            Self::Restarting => "restarting",
            Self::Stopped => "stopped",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ParticipantLaunchCommand {
    pub command_line: String,
    pub env: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ParticipantStatus {
    pub id: String,
    pub kind: ParticipantKind,
    /// Whether this participant runs from a locally resolved directory
    /// (user-authored source, or a local path-pin override) rather than a
    /// fetched suite artifact. Orthogonal to `kind` - see
    /// `phoxal_cli_core::session::participant_kind` module docs. Defaults to `false`
    /// (suite) via [`Self::new`]; set explicitly with [`Self::with_local`].
    #[serde(default)]
    pub local: bool,
    pub state: ParticipantState,
    pub restart_count: u32,
    pub note: Option<String>,
    pub last_log_line: Option<String>,
    pub last_log_lines: Vec<String>,
    pub launch_command: Option<ParticipantLaunchCommand>,
    /// Live child-process details for the interactive session only. These are
    /// intentionally excluded from the persisted board shape.
    #[serde(skip)]
    pub pid: Option<u32>,
    #[serde(skip)]
    pub artifact_size_bytes: Option<u64>,
    /// Current robot-bus Liveliness observation for presentation only. `None`
    /// means the observer has not established graph state in this epoch.
    #[serde(skip)]
    pub present: Option<bool>,
    /// Robot identity for scoped live telemetry lookup. Session-only because
    /// persisted board rows predate and must not become a telemetry index.
    #[serde(skip)]
    pub scope: Option<RobotScope>,
}

impl ParticipantStatus {
    #[must_use]
    pub fn new(id: impl Into<String>, kind: ParticipantKind, state: ParticipantState) -> Self {
        Self {
            id: id.into(),
            kind,
            local: false,
            state,
            restart_count: 0,
            note: None,
            last_log_line: None,
            last_log_lines: Vec::new(),
            launch_command: None,
            pid: None,
            artifact_size_bytes: None,
            present: None,
            scope: None,
        }
    }

    #[must_use]
    pub fn with_local(mut self, local: bool) -> Self {
        self.local = local;
        self
    }

    #[must_use]
    pub fn with_scope(mut self, scope: RobotScope) -> Self {
        self.scope = Some(scope);
        self
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct BoardSnapshot {
    pub participants: BTreeMap<String, ParticipantStatus>,
}
