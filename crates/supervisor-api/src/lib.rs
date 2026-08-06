//! # phoxal-supervisor-api
//!
//! The supervisor's process-boundary contract: connection, identity,
//! lifecycle/process state, commands, bundle files, logs, and telemetry.
//! Shared by `phoxal`, `phoxald`, and the future Operator client.
//!
//! This crate owns **documents, topics, bounds, and schema descriptors**. It
//! owns no sessions, tasks, process manager, daemon state, or UI state - the
//! attachment behavior lives in `phoxal-supervisor-client`, and the authority
//! that produces these documents lives in `phoxald`.
//!
//! It is deliberately not part of `phoxal-api`: that crate owns the
//! robot-domain contract, whose keys carry the API revision
//! (`v0.1/drive/state`) and whose high-rate bodies carry no schema tag. The
//! two are different contracts with different versioning rules, so they are
//! different crates.
//!
//! # The protocol tree
//!
//! The surface is authored in `phoxal_api_tree!`'s **protocol mode**: one flat
//! tree, no revision axis, relative keys, and developer-owned schema-tagged
//! bodies. The protocol name is the leading key segment, so every key here is
//! `supervisor/...` relative and composes under the host's execution-scoped bus
//! root to `phoxal/{execution_id}/supervisor/...` - which is what
//! [`phoxal_bus::Bus::full_key`] does with it.
//!
//! ```text
//! supervisor/connect            query   connect::Request  => connect::Reply
//! supervisor/identity           liveliness token (see `identity_key`)
//! supervisor/snapshot           diagnostic snapshot::Update
//! supervisor/snapshot/current   query   snapshot::CurrentRequest => snapshot::Current
//! supervisor/command            query   command::Request  => command::Reply
//! supervisor/bundle/get         query   bundle::GetRequest => bundle::GetReply
//! supervisor/logs/snapshot      query   logs::SnapshotRequest => logs::Snapshot
//! supervisor/logs/follow        diagnostic logs::Follow
//! supervisor/telemetry/snapshot query   telemetry::SnapshotRequest => telemetry::Snapshot
//! supervisor/telemetry/follow   diagnostic telemetry::Follow
//! ```
//!
//! Every document is a serde-tagged enum whose variant *is* its schema version,
//! and every tag **mirrors the routing above**:
//! `phoxal/<relative-topic-path>[/<role>]/v0`. A tag read out of a capture
//! therefore names the exact endpoint that produced it.
//!
//! Tags are unique **per structure**, not per topic: a request and its reply
//! are different documents and never share a tag, which is what makes a tag
//! identifying rather than merely descriptive. The role suffix disambiguates
//! the structures sharing one topic, and the topic's primary document - the
//! stream, or the reply for a query-only topic - carries no suffix. This is
//! also why the snapshot stream and the `current` query, which carry the same
//! [`model::Snapshot`] payload, are two distinct tags.
//!
//! Pre-v1 the current `V0` is edited in place and every binary is rebuilt; the
//! macro neither infers a breaking change nor mints a version.
//!
//! The daemon publishes with the `diagnostic` role rather than `state`:
//! `phoxald` is not a clocked graph participant, so it has no step token to
//! stamp state with, and none of these documents expresses robot time.
//!
//! # Version identity is a type, not a string
//!
//! Every version this contract names - each document's schema, the bus ABI, the
//! authored robot grammar, the robot API revision - is a serde enum whose
//! variant rename is the canonical text, and that text exists exactly once in
//! this crate. Nothing here holds a `SchemaId`/`ApiId` string newtype, and
//! nothing compares two strings to decide compatibility: a foreign version
//! fails to *deserialize*, and serde's error already names the tag it found and
//! the set it expected. See [`schemas`].
//!
//! Document **tags** are not bus **keys**. Key building is untouched by any of
//! this: robot-domain topics still route on the bare revision (`v0.1/...`) and
//! this protocol on `supervisor/...`.
//!
//! # NEEDED-FROM-FRAMEWORK
//!
//! - **The framework's own version identities should be enums too.**
//!   `phoxal-bus` publishes the bus ABI as `BUS_ABI: &str` and `phoxal-api`
//!   publishes the revision as `ApiVersion::ID: &str`, so [`schemas::BusAbi`]
//!   and [`schemas::RobotApi`] are declared here and pinned to those constants
//!   by test. The framework-owned enums replace them on the next train.
//!   `phoxal_runtime_contract::{ApiId, SchemaId}` are then unnecessary on this
//!   contract - which is just as well, since they are `Deserialize`-only and a
//!   connect reply must be serialized.
//! - `phoxal-api` imports `phoxal_api_tree!` privately (`use
//!   phoxal_macros::phoxal_api_tree;`), so a downstream protocol tree cannot
//!   reach it through the crate that documents it and must depend on
//!   `phoxal-macros` directly. A `pub use` in `phoxal-api` would make the macro
//!   reachable where its documentation lives.

pub mod bounds;
pub mod model;
pub mod schemas;
pub mod text;

pub use bounds::{BoundsError, validate_bundle_path, validate_snapshot};
pub use model::{
    BundleGetOutcome, BundlePathRejection, Command, CommandOutcome, CommandRejection,
    ComponentBinding, Cursor, DaemonFailure, DaemonFailureReason, DesiredState, ExecutionMode,
    ExitStatus, Lifecycle, LogLevel, LogRecord, LogValue, Process, ProcessFailure,
    ProcessFailureKind, ProcessKey, ProcessState, RobotIdentity, RuntimeBufferKind,
    RuntimeDirection, RuntimeStep, RuntimeTopic, Snapshot, StartupRequirement, StartupStep,
    StartupStepKind, StartupStepState, TelemetryRecord, WallTime,
};
pub use schemas::{
    BundleGetSchema, BusAbi, CommandSchema, LogsSchema, RobotApi, RobotDocumentSchema,
    SnapshotSchema, SupervisorSchemas, TelemetrySchema,
};
pub use text::{Bounded, BundlePath, Detail, LogText, Name, StderrTail, TextTooLong};

use phoxal_runtime_contract::ExecutionId;

/// The first chunk of every Phoxal bus key.
///
/// `phoxal-bus` composes the same prefix privately; this crate needs it to
/// state the liveliness token as an exact string. The
/// `identity_key_composes_under_a_live_bus_root` test pins the two together.
const BUS_KEY_PREFIX: &str = "phoxal";

/// The protocol-relative liveliness key the supervisor declares its token on.
///
/// A liveliness token is not a topic, so it is not a leaf in the tree: nothing
/// is ever published or queried here. Its loss is the disconnection signal.
pub const IDENTITY_KEY: &str = "supervisor/identity";

/// The exact liveliness token for one execution:
/// `phoxal/{execution_id}/supervisor/identity`.
#[must_use]
pub fn identity_key(execution: ExecutionId) -> String {
    format!("{BUS_KEY_PREFIX}/{execution}/{IDENTITY_KEY}")
}

/// The same token below an already-composed bus root
/// ([`phoxal_bus::Bus::root`]), for a caller that holds a session.
#[must_use]
pub fn identity_key_under(root: &str) -> String {
    format!("{root}/{IDENTITY_KEY}")
}

phoxal_macros::phoxal_api_tree! {
    protocol supervisor {
        connect {
            /// A client's attachment request. It carries nothing: the client
            /// has already learned the execution from the router it is
            /// connected to, and it asserts no capability it wants the daemon
            /// to trust.
            #[serde(tag = "schema")]
            enum Request {
                #[serde(rename = "phoxal/supervisor/connect/request/v0")]
                V0 {},
            }

            /// What an attaching client needs and nothing else.
            ///
            /// No framework version or train, no router ZID, no execution id,
            /// no supervisor generation, no manifest, no participant list, no
            /// plan digest, no catalog information, and no internal participant
            /// schema inventory. The execution is known from the router and the
            /// key; the process set comes from the snapshot; the manifest comes
            /// from `bundle/get`; the internal schemas are daemon and build
            /// checks that never reach an attachment.
            #[serde(tag = "schema")]
            enum Reply {
                #[serde(rename = "phoxal/supervisor/connect/reply/v0")]
                V0 {
                    robot: crate::model::RobotIdentity,
                    api: crate::schemas::RobotApi,
                    schemas: crate::schemas::SupervisorSchemas,
                    mode: crate::model::ExecutionMode,
                },
            }

            topic self: query Request => Reply;
        }

        snapshot {
            /// The pushed snapshot stream. A client subscribes here *before*
            /// querying `current`, then keeps the highest revision it has seen,
            /// so the query's answer can never be older than the stream it
            /// installed.
            #[serde(tag = "schema")]
            enum Update {
                #[serde(rename = "phoxal/supervisor/snapshot/v0")]
                V0(crate::model::Snapshot),
            }

            topic self: diagnostic Update;

            /// Asks for the authoritative current snapshot. It carries no
            /// filter: the whole execution state is one bounded document.
            #[serde(tag = "schema")]
            enum CurrentRequest {
                #[serde(rename = "phoxal/supervisor/snapshot/current/request/v0")]
                V0 {},
            }

            /// The same document the stream carries, on its own key so a
            /// client can ask for it instead of waiting for the next push.
            #[serde(tag = "schema")]
            enum Current {
                #[serde(rename = "phoxal/supervisor/snapshot/current/v0")]
                V0(crate::model::Snapshot),
            }

            topic current: query CurrentRequest => Current;
        }

        command {
            #[serde(tag = "schema")]
            enum Request {
                #[serde(rename = "phoxal/supervisor/command/request/v0")]
                V0 { command: crate::model::Command },
            }

            /// Accepted or rejected with a typed reason. There is no
            /// command-session, sequence, or resume surface: a query is
            /// already one request with one answer.
            #[serde(tag = "schema")]
            enum Reply {
                #[serde(rename = "phoxal/supervisor/command/reply/v0")]
                V0 {
                    outcome: crate::model::CommandOutcome,
                },
            }

            topic self: query Request => Reply;
        }

        bundle {
            /// Reads one file from the finalized bundle by its
            /// bundle-relative path.
            #[serde(tag = "schema")]
            enum GetRequest {
                #[serde(rename = "phoxal/supervisor/bundle/get/request/v0")]
                V0 { path: crate::text::BundlePath },
            }

            #[serde(tag = "schema")]
            enum GetReply {
                #[serde(rename = "phoxal/supervisor/bundle/get/reply/v0")]
                V0 {
                    outcome: crate::model::BundleGetOutcome,
                },
            }

            topic get: query GetRequest => GetReply;
        }

        logs {
            /// One backward page of the supervisor's bounded log history.
            #[serde(tag = "schema")]
            enum SnapshotRequest {
                #[serde(rename = "phoxal/supervisor/logs/snapshot/request/v0")]
                V0 {
                    /// Exact participant filter. `None` selects every
                    /// participant.
                    participant: Option<crate::text::Name>,
                    /// Newest records to return. Zero selects the supervisor's
                    /// bounded default.
                    limit: u32,
                    /// Exclusive ingest-sequence upper bound for backward
                    /// pagination. `None` starts at the newest record.
                    before_sequence: Option<u64>,
                },
            }

            #[serde(tag = "schema")]
            enum Snapshot {
                #[serde(rename = "phoxal/supervisor/logs/snapshot/v0")]
                V0 {
                    cursor: crate::model::Cursor,
                    /// Cumulative samples the supervisor's own bounded ingest
                    /// subscriber lost. An increase is unrecoverable source
                    /// loss, distinct from a producer's `LogRecord::dropped`.
                    ingest_dropped: u64,
                    records: Vec<crate::model::LogRecord>,
                    /// Pass as the next request's `before_sequence`. `None`
                    /// means the retained matching history is complete.
                    next_before_sequence: Option<u64>,
                },
            }

            /// One live record following the snapshot. A consumer re-queries
            /// when the cursor generation changes or the sequence is not
            /// exactly one past its installed cursor.
            #[serde(tag = "schema")]
            enum Follow {
                #[serde(rename = "phoxal/supervisor/logs/follow/v0")]
                V0 {
                    cursor: crate::model::Cursor,
                    ingest_dropped: u64,
                    record: crate::model::LogRecord,
                },
            }

            topic snapshot: query SnapshotRequest => Snapshot;
            topic follow: diagnostic Follow;
        }

        telemetry {
            #[serde(tag = "schema")]
            enum SnapshotRequest {
                #[serde(rename = "phoxal/supervisor/telemetry/snapshot/request/v0")]
                V0 {
                    participant: Option<crate::text::Name>,
                    limit: u32,
                    before_sequence: Option<u64>,
                },
            }

            #[serde(tag = "schema")]
            enum Snapshot {
                #[serde(rename = "phoxal/supervisor/telemetry/snapshot/v0")]
                V0 {
                    cursor: crate::model::Cursor,
                    records: Vec<crate::model::TelemetryRecord>,
                    /// Records evicted by the memory cap before their age
                    /// horizon elapsed.
                    capacity_evictions: u64,
                    next_before_sequence: Option<u64>,
                },
            }

            #[serde(tag = "schema")]
            enum Follow {
                #[serde(rename = "phoxal/supervisor/telemetry/follow/v0")]
                V0 {
                    cursor: crate::model::Cursor,
                    record: crate::model::TelemetryRecord,
                },
            }

            topic snapshot: query SnapshotRequest => Snapshot;
            topic follow: diagnostic Follow;
        }
    }
}

#[cfg(test)]
mod tests;
