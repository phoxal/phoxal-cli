//! One expected runtime, as a client renders it.

use std::collections::BTreeMap;
use std::path::PathBuf;

use phoxal_client::supervisor::execution::{Process, ProcessState, Snapshot};
use phoxal_runtime_contract::identity::ParticipantId;
use phoxal_runtime_contract::metadata::ParticipantKind;

/// How much of a robot is up, read off one snapshot.
///
/// Every account an operator gets of an incomplete robot - the install report,
/// a startup timeout, the runtimes line on the checklist, the warning `start`
/// leaves behind - is this one split, so what counts as absent and how absence
/// is named are decided once rather than in four places that can disagree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphSplit {
    /// How many of the robot's expected runtimes the supervisor sees.
    pub present: usize,
    /// The ones it does not, in the order the snapshot lists them.
    pub absent: Vec<ParticipantId>,
}

impl GraphSplit {
    /// Every runtime the robot's bundle declares, present or not.
    #[must_use]
    pub const fn expected(&self) -> usize {
        self.present + self.absent.len()
    }

    /// The absent runtimes as the one line every report names them on, or
    /// `None` when the whole robot is present.
    #[must_use]
    pub fn absent_line(&self) -> Option<String> {
        (!self.absent.is_empty()).then(|| {
            self.absent
                .iter()
                .map(ParticipantId::as_str)
                .collect::<Vec<_>>()
                .join(", ")
        })
    }
}

impl From<&Snapshot> for GraphSplit {
    fn from(snapshot: &Snapshot) -> Self {
        Self {
            present: snapshot
                .processes
                .iter()
                .filter(|process| process.state == ProcessState::Present)
                .count(),
            absent: snapshot
                .processes
                .iter()
                .filter(|process| process.state != ProcessState::Present)
                .map(|process| process.participant.clone())
                .collect(),
        }
    }
}

/// What the client that launched a runtime knows about its own child.
///
/// The supervisor can only say *absent*; it watches leases and never started
/// anything. Whoever did start the process is the only one who can say why it
/// is not there, so a session that launched its runtimes carries this beside
/// the snapshot row and an attachment to somebody else's execution does not.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LocalRuntime {
    pub state: LocalRuntimeState,
    /// Where this session retained the runtime's output.
    pub log: PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LocalRuntimeState {
    /// The child is alive as far as this client can tell.
    Running,
    /// The child exited, with the status the operating system reported.
    Exited { status: String },
}

impl LocalRuntimeState {
    /// The short token a process row shows beside an absent runtime.
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            Self::Running => "running".to_string(),
            Self::Exited { status } => status.clone(),
        }
    }
}

/// Every local child fact one session has, keyed by participant id.
pub type LocalRuntimes = BTreeMap<ParticipantId, LocalRuntime>;

/// A snapshot row plus what this client knows locally about it.
///
/// The row itself is the supervisor's authoritative value and is carried whole
/// rather than destructured: adding a field to the contract must not mean
/// editing a projection here. `local` is present exactly when this client
/// launched the runtime itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessObservation {
    pub row: Process,
    pub local: Option<LocalRuntime>,
}

impl ProcessObservation {
    /// The participant kind this row denotes, which its key already carries.
    #[must_use]
    pub const fn kind(&self) -> ParticipantKind {
        self.row.kind
    }
}

pub type ProcessTable = BTreeMap<ParticipantId, ProcessObservation>;

#[cfg(test)]
mod tests {
    use phoxal_client::supervisor::execution::Lifecycle;
    use phoxal_runtime_contract::identity::RobotId;

    use super::*;

    /// Absence is whatever the supervisor does not see, counted once and named
    /// in snapshot order so every report that quotes it agrees.
    #[test]
    fn the_split_counts_what_is_present_and_names_what_is_not() {
        let process = |name: &str, state| Process {
            participant: ParticipantId::new(name).expect("fixture participant"),
            kind: ParticipantKind::Driver,
            state,
            producer: None,
        };
        let mut snapshot = Snapshot {
            robot: RobotId::new("rover").expect("fixture robot"),
            revision: 1,
            lifecycle: Lifecycle::Starting,
            startup: Vec::new(),
            processes: vec![
                process("brain", ProcessState::Present),
                process("front_camera", ProcessState::Absent),
                process("imu", ProcessState::Absent),
            ],
        };

        let split = GraphSplit::from(&snapshot);
        assert_eq!(split.present, 1);
        assert_eq!(split.expected(), 3);
        assert_eq!(split.absent_line().as_deref(), Some("front_camera, imu"));

        snapshot
            .processes
            .retain(|process| process.state == ProcessState::Present);
        let whole = GraphSplit::from(&snapshot);
        assert_eq!(whole.expected(), 1);
        assert_eq!(whole.absent_line(), None);
    }
}
