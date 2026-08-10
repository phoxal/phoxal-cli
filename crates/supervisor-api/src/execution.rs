//! Versioned supervisor execution state and acknowledged operations.

use phoxal_runtime_contract::identity::{ComponentInstanceId, ParticipantId, ProducerId};
use phoxal_runtime_contract::metadata::{MAX_RUNTIME_PARTICIPANTS, ParticipantKind};
use serde::{Deserialize, Deserializer, Serialize};

/// Maximum process rows in one snapshot, identical to the runtime bundle cap.
pub const MAX_PROCESSES: usize = MAX_RUNTIME_PARTICIPANTS;
/// Maximum UTF-8 bytes in one short diagnostic detail.
pub const MAX_DETAIL_BYTES: usize = 1024;
/// Maximum UTF-8 bytes in one retained standard-error tail.
pub const MAX_STDERR_TAIL_BYTES: usize = 8 * 1024;

/// Bounded diagnostic text. Construction truncates; decoding rejects an
/// over-budget peer so every valid value remains deliverable on the bus.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct DiagnosticText<const MAX: usize>(String);

impl<const MAX: usize> DiagnosticText<MAX> {
    /// Bound a diagnostic string, truncating it deterministically when needed.
    #[must_use]
    pub fn new(value: impl AsRef<str>) -> Self {
        Self::new_with_truncation(value).0
    }

    #[must_use]
    /// Bound a diagnostic string and return the number of source bytes removed.
    pub fn new_with_truncation(value: impl AsRef<str>) -> (Self, u32) {
        let value = value.as_ref();
        if value.len() <= MAX {
            return (Self(value.to_string()), 0);
        }
        const MARKER: &str = "...";
        let budget = MAX.saturating_sub(MARKER.len());
        let mut end = budget.min(value.len());
        while !value.is_char_boundary(end) {
            end = end.saturating_sub(1);
        }
        let mut bounded = value[..end].to_string();
        if MAX >= MARKER.len() {
            bounded.push_str(MARKER);
        }
        let dropped = value.len().saturating_sub(end);
        (Self(bounded), u32::try_from(dropped).unwrap_or(u32::MAX))
    }

    #[must_use]
    /// Borrow the bounded UTF-8 text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<const MAX: usize> std::fmt::Display for DiagnosticText<MAX> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl<'de, const MAX: usize> Deserialize<'de> for DiagnosticText<MAX> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        if value.len() > MAX {
            return Err(serde::de::Error::custom(format!(
                "diagnostic text is {} bytes; limit is {MAX}",
                value.len()
            )));
        }
        Ok(Self(value))
    }
}

pub type Detail = DiagnosticText<MAX_DETAIL_BYTES>;
pub type StderrTail = DiagnosticText<MAX_STDERR_TAIL_BYTES>;

/// What the supervisor intends for one persisted participant.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DesiredState {
    Running,
    /// Persisted projection-only state in this protocol revision. No remote
    /// command changes one participant's desired state.
    Stopped,
}

/// What the supervisor currently observes for one persisted participant.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessState {
    Starting,
    Ready,
    Degraded,
    Restarting,
    Stopping,
    Failed,
    Stopped,
}

/// The process operation that produced the most recent failure.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessFailureKind {
    Spawn,
    Exit,
    ReadinessTimeout,
    ReadinessConflict,
    Cleanup,
    ControlPlane,
    #[serde(other)]
    Unknown,
}

/// One unambiguous host exit cause.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "exit", rename_all = "snake_case")]
pub enum ExitStatus {
    Code { code: i32 },
    Signal { signal: i32 },
}

/// A normalized wall-clock diagnostic instant. Never used for runtime order.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WallTime {
    unix_seconds: i64,
    nanos: u32,
}

impl WallTime {
    /// Construct a normalized wall-clock value.
    pub fn new(unix_seconds: i64, nanos: u32) -> Result<Self, WallTimeError> {
        if nanos >= 1_000_000_000 {
            return Err(WallTimeError { nanos });
        }
        Ok(Self {
            unix_seconds,
            nanos,
        })
    }

    #[must_use]
    pub const fn unix_seconds(self) -> i64 {
        self.unix_seconds
    }

    #[must_use]
    pub const fn nanos(self) -> u32 {
        self.nanos
    }

    /// Convert a host wall-clock instant into the normalized wire form.
    #[must_use]
    pub fn from_system_time(value: std::time::SystemTime) -> Self {
        match value.duration_since(std::time::SystemTime::UNIX_EPOCH) {
            Ok(since) => Self {
                unix_seconds: i64::try_from(since.as_secs()).unwrap_or(i64::MAX),
                nanos: since.subsec_nanos(),
            },
            Err(before) => {
                let before = before.duration();
                Self {
                    unix_seconds: i64::try_from(before.as_secs())
                        .map_or(i64::MIN, |seconds| -seconds - 1),
                    nanos: 1_000_000_000 - before.subsec_nanos().max(1),
                }
            }
        }
    }
}

impl<'de> Deserialize<'de> for WallTime {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            unix_seconds: i64,
            nanos: u32,
        }
        let wire = Wire::deserialize(deserializer)?;
        Self::new(wire.unix_seconds, wire.nanos).map_err(serde::de::Error::custom)
    }
}

/// Evidence for one participant incarnation's most recent failure.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessFailure {
    pub kind: ProcessFailureKind,
    pub occurred_at: WallTime,
    pub exit: Option<ExitStatus>,
    pub detail: Detail,
    pub stderr_tail: Option<StderrTail>,
    /// Source bytes removed while bounding `detail` and `stderr_tail`.
    pub truncated_bytes: u32,
}

impl ProcessFailure {
    #[must_use]
    pub fn new(
        kind: ProcessFailureKind,
        occurred_at: WallTime,
        exit: Option<ExitStatus>,
        detail: impl AsRef<str>,
        stderr_tail: Option<impl AsRef<str>>,
    ) -> Self {
        let (detail, detail_truncated) = Detail::new_with_truncation(detail);
        let (stderr_tail, stderr_truncated) = match stderr_tail {
            Some(value) => {
                let (value, truncated) = StderrTail::new_with_truncation(value);
                (Some(value), truncated)
            }
            None => (None, 0),
        };
        Self {
            kind,
            occurred_at,
            exit,
            detail,
            stderr_tail,
            truncated_bytes: detail_truncated.saturating_add(stderr_truncated),
        }
    }
}

/// One process selected by the persisted runtime document.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Process {
    pub participant: ParticipantId,
    pub kind: ParticipantKind,
    pub component: Option<ComponentInstanceId>,
    pub desired: DesiredState,
    pub state: ProcessState,
    pub pid: Option<u32>,
    /// The exact Ready producer for this incarnation, absent until observed.
    pub producer: Option<ProducerId>,
    pub restarts: u64,
    pub failure: Option<ProcessFailure>,
}

/// Whole-execution lifecycle owned by the supervisor.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Lifecycle {
    Starting,
    Ready,
    Degraded,
    Failed,
    Stopping,
    Stopped,
}

/// The fixed supervisor bootstrap sequence.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StartupStepKind {
    Bundle,
    Router,
    Participants,
}

impl StartupStepKind {
    pub const ALL: [Self; 3] = [Self::Bundle, Self::Router, Self::Participants];
}

/// State of one supervisor startup step.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StartupStepState {
    Pending,
    Active,
    Done,
    Failed,
}

/// Progress for one fixed supervisor startup step.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct StartupStep {
    pub kind: StartupStepKind,
    pub state: StartupStepState,
    pub detail: Option<Detail>,
    pub elapsed_ms: Option<u64>,
}

/// Why the execution failed when no single participant row is sufficient.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DaemonFailureReason {
    InvalidBundle,
    RouterUnavailable,
    RouterLost,
    LaunchFailed,
    TeardownFailed,
    ControlPlaneLost,
    Internal,
    #[serde(other)]
    Unknown,
}

/// Whole-execution failure evidence.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DaemonFailure {
    pub reason: DaemonFailureReason,
    pub detail: Detail,
}

impl DaemonFailure {
    #[must_use]
    pub fn new(reason: DaemonFailureReason, detail: impl AsRef<str>) -> Self {
        Self {
            reason,
            detail: Detail::new(detail),
        }
    }
}

/// Complete mutable supervisor projection at one execution-local revision.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Snapshot {
    /// Strictly increasing within one execution and incomparable across runs.
    pub revision: u64,
    pub lifecycle: Lifecycle,
    pub startup: Vec<StartupStep>,
    /// Ordered by participant id by the daemon projection.
    pub processes: Vec<Process>,
    pub failure: Option<DaemonFailure>,
}

impl Snapshot {
    /// Verify ordering, bounds, and lifecycle relationships before publication.
    pub fn validate(&self) -> Result<(), SnapshotError> {
        if self.processes.len() > MAX_PROCESSES {
            return Err(SnapshotError::TooManyProcesses {
                count: self.processes.len(),
            });
        }
        if self.startup.len() > StartupStepKind::ALL.len() {
            return Err(SnapshotError::TooManyStartupSteps {
                count: self.startup.len(),
            });
        }
        for (index, pair) in self.processes.windows(2).enumerate() {
            match pair[0].participant.cmp(&pair[1].participant) {
                std::cmp::Ordering::Less => {}
                std::cmp::Ordering::Equal => {
                    return Err(SnapshotError::DuplicateParticipant { index: index + 1 });
                }
                std::cmp::Ordering::Greater => {
                    return Err(SnapshotError::UnorderedParticipants { index: index + 1 });
                }
            }
        }
        for (index, step) in self.startup.iter().enumerate() {
            let expected = StartupStepKind::ALL[index];
            if step.kind != expected {
                if self.startup[..index]
                    .iter()
                    .any(|previous| previous.kind == step.kind)
                {
                    return Err(SnapshotError::DuplicateStartupStep { kind: step.kind });
                }
                return Err(SnapshotError::UnorderedStartupStep {
                    index,
                    expected,
                    actual: step.kind,
                });
            }
        }
        for process in &self.processes {
            if (process.kind == ParticipantKind::Driver) != process.component.is_some() {
                return Err(SnapshotError::InvalidProcessComponent {
                    participant: process.participant.clone(),
                    kind: process.kind,
                });
            }
            if process.state == ProcessState::Failed && process.failure.is_none() {
                return Err(SnapshotError::MissingProcessFailure {
                    participant: process.participant.clone(),
                });
            }
            if process.state == ProcessState::Ready && process.producer.is_none() {
                return Err(SnapshotError::MissingReadyProducer {
                    participant: process.participant.clone(),
                });
            }
        }
        if self.lifecycle == Lifecycle::Failed && self.failure.is_none() {
            return Err(SnapshotError::MissingDaemonFailure);
        }
        if matches!(self.lifecycle, Lifecycle::Ready | Lifecycle::Degraded)
            && self.failure.is_some()
        {
            return Err(SnapshotError::UnexpectedDaemonFailure);
        }
        Ok(())
    }
}

impl<'de> Deserialize<'de> for Snapshot {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            revision: u64,
            lifecycle: Lifecycle,
            startup: Vec<StartupStep>,
            processes: Vec<Process>,
            failure: Option<DaemonFailure>,
        }
        let wire = Wire::deserialize(deserializer)?;
        let snapshot = Self {
            revision: wire.revision,
            lifecycle: wire.lifecycle,
            startup: wire.startup,
            processes: wire.processes,
            failure: wire.failure,
        };
        snapshot.validate().map_err(serde::de::Error::custom)?;
        Ok(snapshot)
    }
}

impl Serialize for Snapshot {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.validate().map_err(serde::ser::Error::custom)?;
        #[derive(Serialize)]
        #[serde(deny_unknown_fields)]
        struct Wire<'a> {
            revision: u64,
            lifecycle: Lifecycle,
            startup: &'a [StartupStep],
            processes: &'a [Process],
            failure: &'a Option<DaemonFailure>,
        }
        Wire {
            revision: self.revision,
            lifecycle: self.lifecycle,
            startup: &self.startup,
            processes: &self.processes,
            failure: &self.failure,
        }
        .serialize(serializer)
    }
}

/// Versioned snapshot wire document.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "schema")]
pub enum SnapshotDocument {
    #[serde(rename = "phoxal/supervisor-snapshot/v0")]
    V0(Snapshot),
}

impl SnapshotDocument {
    #[must_use]
    pub const fn snapshot(&self) -> &Snapshot {
        match self {
            Self::V0(snapshot) => snapshot,
        }
    }

    #[must_use]
    pub fn into_snapshot(self) -> Snapshot {
        match self {
            Self::V0(snapshot) => snapshot,
        }
    }
}

impl<'de> Deserialize<'de> for SnapshotDocument {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(tag = "schema")]
        enum Wire {
            #[serde(rename = "phoxal/supervisor-snapshot/v0")]
            V0(Snapshot),
        }
        match Wire::deserialize(deserializer)? {
            Wire::V0(snapshot) => Ok(Self::V0(snapshot)),
        }
    }
}

/// One acknowledged operation requested from supervisor authority.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum Command {
    /// Compare-and-swap the observed Ready producer. `None` restarts only when
    /// the supervisor also observes no Ready producer.
    Restart {
        participant: ParticipantId,
        expected_producer: Option<ProducerId>,
    },
    /// Idempotently stop this execution.
    Stop,
}

/// Outcome of one acknowledged supervisor command.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum CommandOutcome {
    /// The snapshot revision observed by the authority when it accepted the command.
    Accepted {
        at_revision: u64,
    },
    Rejected {
        reason: CommandRejection,
    },
}

/// Typed command rejection suitable for caller branching.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandRejection {
    UnknownParticipant,
    ProducerFenced,
    Busy,
    ControlClosed,
    #[serde(other)]
    Unknown,
}

/// A snapshot that violates the supervisor wire invariants.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
#[non_exhaustive]
pub enum SnapshotError {
    #[error("snapshot has {count} processes; limit is {MAX_PROCESSES}")]
    TooManyProcesses { count: usize },
    #[error("snapshot has {count} startup steps; limit is {}", StartupStepKind::ALL.len())]
    TooManyStartupSteps { count: usize },
    #[error("snapshot repeats participant at index {index}")]
    DuplicateParticipant { index: usize },
    #[error("snapshot processes are unordered at index {index}")]
    UnorderedParticipants { index: usize },
    #[error("snapshot repeats startup step {kind:?}")]
    DuplicateStartupStep { kind: StartupStepKind },
    #[error("snapshot startup step {index} is {actual:?}; expected {expected:?}")]
    UnorderedStartupStep {
        index: usize,
        expected: StartupStepKind,
        actual: StartupStepKind,
    },
    #[error("participant {participant} has an invalid component for kind {kind:?}")]
    InvalidProcessComponent {
        participant: ParticipantId,
        kind: ParticipantKind,
    },
    #[error("failed participant {participant} carries no failure evidence")]
    MissingProcessFailure { participant: ParticipantId },
    #[error("ready participant {participant} carries no producer")]
    MissingReadyProducer { participant: ParticipantId },
    #[error("failed lifecycle carries no daemon failure")]
    MissingDaemonFailure,
    #[error("running lifecycle carries stale daemon failure evidence")]
    UnexpectedDaemonFailure,
}

/// A wall-clock value whose nanosecond component is not normalized.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("wall-time nanos must be below 1,000,000,000, got {nanos}")]
pub struct WallTimeError {
    nanos: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn process(id: &str) -> Process {
        Process {
            participant: ParticipantId::new(id).expect("valid participant id"),
            kind: ParticipantKind::Brain,
            component: None,
            desired: DesiredState::Running,
            state: ProcessState::Starting,
            pid: Some(42),
            producer: None,
            restarts: 0,
            failure: None,
        }
    }

    fn snapshot(processes: Vec<Process>) -> Snapshot {
        Snapshot {
            revision: 1,
            lifecycle: Lifecycle::Starting,
            startup: Vec::new(),
            processes,
            failure: None,
        }
    }

    #[test]
    fn process_rows_are_ordered_and_unique() {
        assert_eq!(
            snapshot(vec![process("brain"), process("drive")]).validate(),
            Ok(())
        );
        assert_eq!(
            snapshot(vec![process("drive"), process("brain")]).validate(),
            Err(SnapshotError::UnorderedParticipants { index: 1 })
        );
        assert_eq!(
            snapshot(vec![process("drive"), process("drive")]).validate(),
            Err(SnapshotError::DuplicateParticipant { index: 1 })
        );
    }

    #[test]
    fn snapshot_document_round_trips_and_rejects_unknown_fields() {
        let document = SnapshotDocument::V0(snapshot(vec![process("brain")]));
        let encoded = rmp_serde::to_vec_named(&document).expect("snapshot encodes");
        assert_eq!(
            rmp_serde::from_slice::<SnapshotDocument>(&encoded).expect("snapshot decodes"),
            document
        );
        let malformed = rmp_serde::to_vec_named(&serde_json::json!({
            "schema": "phoxal/supervisor-snapshot/v0",
            "revision": 1,
            "lifecycle": "starting",
            "startup": [],
            "processes": [],
            "failure": null,
            "extra": true
        }))
        .expect("malformed fixture encodes");
        assert!(rmp_serde::from_slice::<SnapshotDocument>(&malformed).is_err());
    }

    #[test]
    fn bounds_and_relational_failures_are_enforced() {
        let mut too_many = snapshot(
            (0..=MAX_PROCESSES)
                .map(|i| process(&format!("p{i}")))
                .collect(),
        );
        too_many
            .processes
            .sort_by(|a, b| a.participant.cmp(&b.participant));
        assert!(matches!(
            too_many.validate(),
            Err(SnapshotError::TooManyProcesses { .. })
        ));

        let mut duplicate_steps = snapshot(Vec::new());
        duplicate_steps.startup = vec![
            StartupStep {
                kind: StartupStepKind::Bundle,
                state: StartupStepState::Done,
                detail: None,
                elapsed_ms: None,
            },
            StartupStep {
                kind: StartupStepKind::Bundle,
                state: StartupStepState::Done,
                detail: None,
                elapsed_ms: None,
            },
        ];
        assert!(matches!(
            duplicate_steps.validate(),
            Err(SnapshotError::DuplicateStartupStep { .. })
        ));
    }

    #[test]
    fn diagnostic_text_is_bounded_and_reports_truncation() {
        let source = "é".repeat(MAX_DETAIL_BYTES);
        let (bounded, dropped) = Detail::new_with_truncation(&source);
        assert!(bounded.as_str().len() <= MAX_DETAIL_BYTES);
        assert!(bounded.as_str().ends_with("..."));
        assert_eq!(
            usize::try_from(dropped).unwrap(),
            source.len() - (MAX_DETAIL_BYTES - 4)
        );

        let encoded = rmp_serde::to_vec_named(&source).unwrap();
        assert!(rmp_serde::from_slice::<Detail>(&encoded).is_err());
    }

    #[test]
    fn every_snapshot_relation_is_enforced_on_validation_and_encoding() {
        let mut ready = process("brain");
        ready.state = ProcessState::Ready;
        let invalid = snapshot(vec![ready]);
        assert!(matches!(
            invalid.validate(),
            Err(SnapshotError::MissingReadyProducer { .. })
        ));
        assert!(rmp_serde::to_vec_named(&invalid).is_err());

        let mut failed_process = process("brain");
        failed_process.state = ProcessState::Failed;
        assert!(matches!(
            snapshot(vec![failed_process]).validate(),
            Err(SnapshotError::MissingProcessFailure { .. })
        ));

        let mut failed_daemon = snapshot(Vec::new());
        failed_daemon.lifecycle = Lifecycle::Failed;
        assert_eq!(
            failed_daemon.validate(),
            Err(SnapshotError::MissingDaemonFailure)
        );

        let mut stale_failure = snapshot(Vec::new());
        stale_failure.lifecycle = Lifecycle::Ready;
        stale_failure.failure = Some(DaemonFailure::new(DaemonFailureReason::Internal, "stale"));
        assert_eq!(
            stale_failure.validate(),
            Err(SnapshotError::UnexpectedDaemonFailure)
        );

        let mut component = process("brain");
        component.component = Some(ComponentInstanceId::new("base").unwrap());
        assert!(matches!(
            snapshot(vec![component]).validate(),
            Err(SnapshotError::InvalidProcessComponent { .. })
        ));

        let mut startup = snapshot(Vec::new());
        startup.startup.push(StartupStep {
            kind: StartupStepKind::Router,
            state: StartupStepState::Pending,
            detail: None,
            elapsed_ms: None,
        });
        assert!(matches!(
            startup.validate(),
            Err(SnapshotError::UnorderedStartupStep { .. })
        ));
    }

    #[test]
    fn restart_fences_both_present_and_absent_producers() {
        for expected_producer in [
            None,
            Some(ProducerId::try_from((1_u128 << 124) | 43).expect("canonical producer id")),
        ] {
            let command = Command::Restart {
                participant: ParticipantId::new("drive").expect("valid participant id"),
                expected_producer,
            };
            let encoded = rmp_serde::to_vec_named(&command).expect("command encodes");
            assert_eq!(
                rmp_serde::from_slice::<Command>(&encoded).expect("command decodes"),
                command
            );
        }
    }

    #[test]
    fn wall_time_rejects_non_normalized_values_at_construction_and_decode() {
        assert!(WallTime::new(1, 1_000_000_000).is_err());
        let encoded = rmp_serde::to_vec_named(&serde_json::json!({
            "unix_seconds": 1,
            "nanos": 1_000_000_000_u32,
        }))
        .expect("fixture encodes");
        assert!(rmp_serde::from_slice::<WallTime>(&encoded).is_err());
    }

    #[test]
    fn startup_all_is_exhaustive() {
        for kind in StartupStepKind::ALL {
            match kind {
                StartupStepKind::Bundle
                | StartupStepKind::Router
                | StartupStepKind::Participants => {}
            }
        }
    }
}
