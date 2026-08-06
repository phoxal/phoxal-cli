//! The supervisor's model vocabulary: the payloads the schema-tagged protocol
//! documents carry.
//!
//! These types live outside the `phoxal_api_tree!` invocation because several
//! of them appear in more than one document (a `ProcessKey` is both a snapshot
//! row's identity and a restart command's target), and a tree node's types are
//! local to that node. The schema-tagged carriers in the tree own the version
//! decision; these own the shape.
//!
//! Closed sets are enumerations, never strings: a client renders a reason, and
//! a reason it cannot match on is a reason it cannot render deliberately.

use std::collections::BTreeMap;

use phoxal_runtime_contract::{ParticipantKind, ProducerId};
use serde::{Deserialize, Serialize};

use crate::text::{Detail, LogText, Name, StderrTail};

/// The stable authored identity of the robot this execution runs.
///
/// Stable identity is model data, never transport routing: every key is rooted
/// at the execution, and this is what a client verifies the local manifest
/// against and what it displays.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RobotIdentity {
    pub id: Name,
    pub namespace: Name,
}

/// Which clock the execution runs on, read straight out of the finalized
/// manifest's `clock:` field.
///
/// The daemon has no simulation branch: this is passthrough, and a client uses
/// it to decide its own behavior (who owns Webots, whether `q` detaches).
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionMode {
    Real,
    Simulated,
}

/// The identity of one supervised process within an execution.
///
/// The variant *is* the participant kind, so there is no second `kind` field to
/// contradict it. A driver and a simulator are both bound to a component
/// instance and are distinguishable only by variant, which is exactly the
/// distinction the daemon derives from the finalized manifest.
#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(tag = "scope", rename_all = "snake_case")]
pub enum ProcessKey {
    /// The one mandatory project-owned root brain.
    Brain,
    /// An official native service or an authored user service.
    Service { id: Name },
    /// A component instance's driver, derived because that instance kept its
    /// `driver:` block through finalization.
    Driver { instance: Name },
    /// A component instance's simulator, derived because the manifest says
    /// `clock: simulated`.
    Simulator { instance: Name },
}

impl ProcessKey {
    /// The participant kind this key denotes.
    #[must_use]
    pub const fn kind(&self) -> ParticipantKind {
        match self {
            Self::Brain => ParticipantKind::Brain,
            Self::Service { .. } => ParticipantKind::Service,
            Self::Driver { .. } => ParticipantKind::Driver,
            Self::Simulator { .. } => ParticipantKind::Simulator,
        }
    }

    /// The component instance this process is bound to, when it is bound to
    /// one.
    #[must_use]
    pub const fn component_instance(&self) -> Option<&Name> {
        match self {
            Self::Brain | Self::Service { .. } => None,
            Self::Driver { instance } | Self::Simulator { instance } => Some(instance),
        }
    }
}

impl std::fmt::Display for ProcessKey {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Brain => formatter.write_str("brain"),
            Self::Service { id } => write!(formatter, "service:{id}"),
            Self::Driver { instance } => write!(formatter, "driver:{instance}"),
            Self::Simulator { instance } => write!(formatter, "simulator:{instance}"),
        }
    }
}

/// The component a driver or simulator process serves. `instance` repeats the
/// key's segment; `component_type` is the frozen definition it resolved to, so
/// a client can render "base (ddsm115)" without reading the manifest.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ComponentBinding {
    pub instance: Name,
    pub component_type: Name,
}

/// Whether a process's absence fails the execution's readiness.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StartupRequirement {
    Required,
    Optional,
}

/// What the supervisor intends for a process.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DesiredState {
    Running,
    Stopped,
}

/// What the supervisor observes of a process.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessState {
    Starting,
    Ready,
    Degraded,
    Restarting,
    Failed,
    Stopped,
}

/// Which step of a process's life failed.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessFailureKind {
    Spawn,
    Exit,
    ReadinessTimeout,
    Cleanup,
}

/// How a process ended, as the host reported it.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExitStatus {
    pub code: Option<i32>,
    pub signal: Option<i32>,
}

/// A wall-clock instant, carried as its own fields rather than a host time
/// type: a client on another machine renders this, and never does arithmetic
/// against its own monotonic clock with it.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WallTime {
    pub unix_seconds: i64,
    pub nanos: u32,
}

/// The evidence for one process's most recent failure.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessFailure {
    pub kind: ProcessFailureKind,
    pub occurred_at: WallTime,
    pub exit: Option<ExitStatus>,
    pub detail: Detail,
    pub stderr_tail: Option<StderrTail>,
}

/// One supervised process, as the daemon authoritatively knows it.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Process {
    pub key: ProcessKey,
    /// Present exactly for the component-bound kinds.
    pub component: Option<ComponentBinding>,
    pub startup: StartupRequirement,
    pub desired: DesiredState,
    pub state: ProcessState,
    pub pid: Option<u32>,
    /// Learned from this incarnation's liveliness token, so it is absent until
    /// the process's own bus session is up. Absent reports a fact - "no session
    /// yet" - rather than an intention, and a restart fenced on an absent
    /// producer is correctly rejected.
    pub producer: Option<ProducerId>,
    /// Restarts within this execution. There is no supervisor generation to
    /// scope a second counter to.
    pub restarts: u64,
    pub failure: Option<ProcessFailure>,
}

/// Where the execution as a whole is.
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

/// The daemon's startup sequence, as a client renders it while waiting.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StartupStepKind {
    /// Lock and validate the finalized bundle.
    Bundle,
    /// Derive the runtime requirements from the manifest and the catalog.
    Requirements,
    /// Validate every selected binary's embedded compatibility metadata.
    Binaries,
    /// Open the embedded router on the execution identity.
    Router,
    /// Launch and prove the participant graph.
    Participants,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StartupStepState {
    Pending,
    Active,
    Done,
    Failed,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct StartupStep {
    pub kind: StartupStepKind,
    pub state: StartupStepState,
    pub detail: Option<Detail>,
    pub elapsed_ms: Option<u64>,
}

/// Why the execution as a whole failed, when no single process is to blame.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DaemonFailureReason {
    /// The bundle root is not a finalized bundle, or its documents do not
    /// validate.
    InvalidBundle,
    /// A selected binary disagrees with this daemon on the API or a schema.
    IncompatibleParticipant,
    /// The embedded router could not be opened or bound.
    RouterUnavailable,
    /// The router this execution *is* went away; the execution ends with it.
    RouterLost,
    /// `clock: simulated` with no world-clock producer - the client-owned
    /// simulator never arrived.
    WorldClockMissing,
    /// A required process could not be launched or never became ready.
    LaunchFailed,
    /// Anything the daemon could not classify. `detail` carries the chain.
    Internal,
}

/// A typed reason plus its rendered evidence.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DaemonFailure {
    pub reason: DaemonFailureReason,
    pub detail: Detail,
}

/// The complete authoritative execution state at one revision.
///
/// Carries no framework compatibility, no supervisor generation, no duplicated
/// manifest, and no plan digest: the manifest comes from `bundle/get`, and
/// compatibility was settled at connect.
///
/// It also carries no manual-input capability. A client derives its own drive
/// parameters from the finalized `robot.yaml` it fetches through `bundle/get`,
/// which keeps kinematics in the one document that owns them instead of
/// republishing a derived copy on every revision.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Snapshot {
    /// Monotonic within one execution. A client keeps the highest it has seen;
    /// there is no generation to compare first.
    pub revision: u64,
    pub robot: RobotIdentity,
    pub mode: ExecutionMode,
    pub lifecycle: Lifecycle,
    pub startup: Vec<StartupStep>,
    /// The actual selected process set, ordered by key.
    pub processes: Vec<Process>,
    pub failure: Option<DaemonFailure>,
}

/// What a client asks the supervisor to do.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum Command {
    /// Restart one process, fenced on the producer the client last saw.
    ///
    /// The supervisor compares `expected_producer` against the producer it
    /// learned from the incarnation's liveliness token. A fresh incarnation
    /// that has not opened its session yet has no producer, so a stale restart
    /// is rejected rather than applied to the wrong incarnation.
    Restart {
        process: ProcessKey,
        expected_producer: ProducerId,
    },
    /// End the execution.
    Stop,
}

/// Whether the supervisor took the command.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum CommandOutcome {
    Accepted,
    Rejected { reason: CommandRejection },
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandRejection {
    /// No process in the current snapshot has that key.
    UnknownProcess,
    /// The target's producer is not the expected one - either it has already
    /// been replaced, or the fresh incarnation has not opened its session yet.
    ProducerFenced,
    /// The execution is already stopping or stopped.
    NotRunning,
}

/// Why a bundle path was refused. Mirrored client-side so an obviously bad
/// path never reaches the wire.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BundlePathRejection {
    Empty,
    Absolute,
    /// Contains a `..` component.
    ParentTraversal,
    /// Contains a `.` component, an empty component, or a backslash: a path
    /// that is not already normalized is refused rather than normalized, so
    /// both ends agree on exactly one spelling per file.
    NotNormalized,
    TooLong,
    /// Resolved outside the bundle root, or through a symlink that leaves it.
    Escapes,
    /// Resolved to something that is not a regular file.
    NotRegularFile,
}

/// The result of one bundle-file read.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum BundleGetOutcome {
    Found {
        #[serde(with = "serde_bytes")]
        bytes: Vec<u8>,
    },
    Missing,
    InvalidPath {
        reason: BundlePathRejection,
    },
    TooLarge {
        size_bytes: u64,
        limit_bytes: u64,
    },
}

/// A collector's identity plus a position in its completed stream.
///
/// `generation` is opaque: consumers compare it for equality only. A change
/// means the collector restarted and the consumer must re-query rather than
/// splice two histories together.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Cursor {
    pub generation: Name,
    pub sequence: u64,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LogLevel {
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

/// A scalar tracing field value.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(untagged)]
pub enum LogValue {
    Bool(bool),
    I64(i64),
    U64(u64),
    F64(f64),
    String(LogText),
}

/// One retained participant log. `sequence` is assigned by the supervisor at
/// ingest and is independent of the producer's own `source_sequence`.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LogRecord {
    pub sequence: u64,
    pub participant: Name,
    pub source_sequence: u64,
    pub time: WallTime,
    pub level: LogLevel,
    pub target: LogText,
    pub message: LogText,
    pub fields: BTreeMap<Name, LogValue>,
    /// Records the producer lost before publication.
    pub dropped: u32,
    /// Values truncated inside this record to keep it bounded.
    pub truncated: u32,
}

/// Which side of one participant-local bus buffer a runtime row measures.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeDirection {
    Publish,
    Subscribe,
    /// The bounded overflow row only, which may combine both directions.
    Mixed,
}

/// The concrete bounded buffer a runtime row measures.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeBufferKind {
    /// Per-topic view of the one shared process outbound queue. Its capacity
    /// is repeated per row and must not be summed.
    Outbound,
    /// Keep-last slot: depth `1` means occupied, never backlog.
    Latest,
    Subscriber,
    /// The bounded overflow row only.
    Mixed,
}

/// Scheduled-step work completed during one rollup window. An unscheduled
/// participant reports `None` instead.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeStep {
    pub target_period_ns: u64,
    pub completed: u64,
    pub errors: u64,
    pub mean_duration_ns: u64,
    pub max_duration_ns: u64,
    pub mean_lateness_ns: u64,
    pub max_lateness_ns: u64,
    pub missed_ticks: u64,
    pub overruns: u64,
}

/// One exact topic/direction/buffer row. An empty `topic` with `Mixed`
/// direction and kind identifies the explicit overflow row.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeTopic {
    pub topic: LogText,
    pub direction: RuntimeDirection,
    pub buffer_kind: RuntimeBufferKind,
    pub count: u64,
    /// Finite and non-negative; the supervisor clamps malformed inputs at
    /// ingest.
    pub rate_hz: f32,
    pub drops: u64,
    pub latest_overwrites: u64,
    pub bounded_evictions: u64,
    pub capacity: u64,
    pub current_depth: u64,
    pub high_water_depth: u64,
    pub decode_errors: u64,
    pub overflowed_rows: u32,
}

/// One retained runtime rollup.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TelemetryRecord {
    pub sequence: u64,
    pub participant: Name,
    pub truncated: u32,
    pub window_ns: u64,
    pub step: Option<RuntimeStep>,
    pub topics: Vec<RuntimeTopic>,
    pub overflow: Option<RuntimeTopic>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_process_keys_variant_is_its_participant_kind() {
        let cases = [
            (ProcessKey::Brain, ParticipantKind::Brain, None),
            (
                ProcessKey::Service {
                    id: Name::new("drive"),
                },
                ParticipantKind::Service,
                None,
            ),
            (
                ProcessKey::Driver {
                    instance: Name::new("base"),
                },
                ParticipantKind::Driver,
                Some("base"),
            ),
            (
                ProcessKey::Simulator {
                    instance: Name::new("base"),
                },
                ParticipantKind::Simulator,
                Some("base"),
            ),
        ];
        for (key, kind, instance) in cases {
            assert_eq!(key.kind(), kind, "{key}");
            assert_eq!(
                key.component_instance().map(Name::as_str),
                instance,
                "{key}"
            );
        }
    }

    #[test]
    fn a_restart_carries_the_producer_it_is_fenced_on() {
        let command = Command::Restart {
            process: ProcessKey::Service {
                id: Name::new("drive"),
            },
            expected_producer: ProducerId::try_from(0x2b).unwrap(),
        };
        let bytes = rmp_serde::to_vec_named(&command).unwrap();
        let decoded: Command = rmp_serde::from_slice(&bytes).unwrap();
        assert_eq!(decoded, command);

        // The fence is not optional in the shape: a restart without it is not
        // a restart this contract can express.
        let json = serde_json::to_value(&command).unwrap();
        assert_eq!(json["command"], "restart");
        assert!(json.get("expected_producer").is_some());
        assert_eq!(json["process"]["scope"], "service");

        let stop = serde_json::to_value(Command::Stop).unwrap();
        assert_eq!(stop["command"], "stop");
    }
}
