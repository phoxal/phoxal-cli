//! Persisted participant-board records and their terminal-neutral text form.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::ParticipantKind;

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
    /// fetched catalog artifact. Orthogonal to `kind` - see
    /// `phoxal_cli_core::session::participant_kind` module docs. Defaults to `false`
    /// (catalog) via [`Self::new`]; set explicitly with [`Self::with_local`].
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
        }
    }

    #[must_use]
    pub fn with_local(mut self, local: bool) -> Self {
        self.local = local;
        self
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct BoardSnapshot {
    pub participants: BTreeMap<String, ParticipantStatus>,
}

impl BoardSnapshot {
    #[must_use]
    pub fn failed_participants(&self) -> Vec<String> {
        self.participants
            .values()
            .filter(|participant| participant.state == ParticipantState::Failed)
            .map(|participant| participant.id.clone())
            .collect()
    }

    #[must_use]
    pub fn has_running_state(&self) -> bool {
        self.participants.values().any(|participant| {
            matches!(
                participant.state,
                ParticipantState::Starting | ParticipantState::Ready | ParticipantState::Restarting
            )
        })
    }

    #[must_use]
    pub fn render(&self) -> String {
        // The kind column carries a trailing `*` for a participant running
        // from a local directory (user-authored source, or a local path-pin
        // override) rather than a fetched catalog artifact - see
        // `ParticipantStatus::local`.
        let mut out = String::from(
            "participant                 kind          state       restarts  note  last log\n",
        );
        out.push_str(
            "--------------------------------------------------------------------------------\n",
        );
        for participant in self.participants.values() {
            let note = participant.note.as_deref().unwrap_or("-");
            let last = participant.last_log_line.as_deref().unwrap_or("-");
            let kind_label = format!(
                "{}{}",
                participant.kind.label(),
                if participant.local { "*" } else { "" }
            );
            out.push_str(&format!(
                "{:<27} {:<13} {:<11} {:>8}  {}  {}\n",
                trim_cell(&participant.id, 27),
                kind_label,
                participant.state.label(),
                participant.restart_count,
                trim_cell(note, 44),
                trim_cell(last, 72),
            ));
        }
        out
    }
}

fn trim_cell(value: &str, width: usize) -> String {
    let count = value.chars().count();
    if count <= width {
        return value.to_string();
    }
    if width <= 1 {
        return ".".to_string();
    }
    value.chars().take(width - 1).collect::<String>() + "."
}
