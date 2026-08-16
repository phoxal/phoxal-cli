//! One expected runtime, as a client renders it.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Instant;

use phoxal_client::supervisor::execution::Process;
use phoxal_runtime_contract::identity::ParticipantId;
use phoxal_runtime_contract::metadata::ParticipantKind;

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
/// editing a projection here. `observed_present_at` is a client-local
/// wall-clock fact - when this client first saw the runtime hold a lease - and
/// `local` is present exactly when this client launched the runtime itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessObservation {
    pub row: Process,
    /// When this client first saw the row present. `None` while it never has.
    pub observed_present_at: Option<Instant>,
    pub local: Option<LocalRuntime>,
}

impl ProcessObservation {
    /// The participant kind this row denotes, which its key already carries.
    #[must_use]
    pub const fn kind(&self) -> ParticipantKind {
        self.row.kind
    }

    /// Whether the runtime holds a Ready lease, which is exactly whether the
    /// supervisor learned a producer from its liveliness token.
    #[must_use]
    pub const fn present(&self) -> bool {
        self.row.producer.is_some()
    }
}

pub type ProcessTable = BTreeMap<ParticipantId, ProcessObservation>;
