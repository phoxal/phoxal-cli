//! Supervisor-owned process protocol.
//!
//! This surface is intentionally independent of the robot API revision. A
//! process can therefore report an unknown-but-valid `RobotApiVersion` without
//! requiring a robot-domain compatibility adapter.
//!
//! The crate root is the canonical facade for execution state and control
//! values. Payload carrier modules remain available for generated endpoint
//! signatures; consumers should import shared values from this root.

#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::todo,
        clippy::unimplemented
    )
)]

use phoxal_macros::phoxal_protocol;
use serde::Deserialize;

pub use phoxal_runtime_contract::clock::Clock;
pub use phoxal_runtime_contract::identity::RobotId;

mod execution;
pub use execution::{
    Command, CommandOutcome, CommandRejection, DaemonFailure, DaemonFailureReason, DesiredState,
    Detail, DiagnosticText, ExitStatus, Lifecycle, MAX_DETAIL_BYTES, MAX_PROCESSES,
    MAX_STDERR_TAIL_BYTES, Process, ProcessFailure, ProcessFailureKind, ProcessState, Snapshot,
    SnapshotDocument, SnapshotError, StartupStep, StartupStepKind, StartupStepState, StderrTail,
    WallTime, WallTimeError,
};

/// Execution-scoped supervisor presence lease. This is a Liveliness key, not
/// an endpoint payload, and therefore deliberately sits outside the endpoint
/// manifest. It composes under one already-known execution root and signals
/// loss of that execution's control-plane authority; it performs no discovery.
pub const PRESENCE_KEY: &str = "supervisor/presence";

pub(crate) fn deserialize_finite_f64<'de, D>(deserializer: D) -> Result<f64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = f64::deserialize(deserializer)?;
    if value.is_finite() {
        Ok(value)
    } else {
        Err(serde::de::Error::custom(
            "expected a finite floating-point value",
        ))
    }
}

/// Ordinary protocol payloads. The generated `supervisor` module below owns
/// endpoint descriptors and topic builders; these modules own only wire data.
pub mod payload {
    pub mod snapshot {
        #[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        pub struct CurrentRequest {}

        /// Query this once before subscribing so a late joiner has a baseline;
        /// then apply every value received from the snapshot stream.
        pub type Current = crate::SnapshotDocument;
        /// A complete replacement snapshot published after the baseline.
        pub type Update = crate::SnapshotDocument;
    }

    pub mod command {
        #[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
        #[serde(tag = "schema")]
        pub enum Request {
            #[serde(rename = "phoxal/supervisor-control/request/v0")]
            V0 { command: crate::Command },
        }

        #[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
        #[serde(tag = "schema")]
        pub enum Reply {
            #[serde(rename = "phoxal/supervisor-control/reply/v0")]
            V0 { outcome: crate::CommandOutcome },
        }

        #[cfg(test)]
        mod tests {
            use super::*;

            #[test]
            fn documents_round_trip_with_explicit_schema_tags() {
                let request = Request::V0 {
                    command: crate::Command::Stop,
                };
                let encoded = rmp_serde::to_vec_named(&request).expect("request encodes");
                assert_eq!(rmp_serde::from_slice::<Request>(&encoded).unwrap(), request);

                let reply = Reply::V0 {
                    outcome: crate::CommandOutcome::Accepted { at_revision: 18 },
                };
                let encoded = rmp_serde::to_vec_named(&reply).expect("reply encodes");
                assert_eq!(rmp_serde::from_slice::<Reply>(&encoded).unwrap(), reply);
            }
        }
    }

    pub mod logs {
        #[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
        pub struct Timestamp {
            pub unix_seconds: i64,
            pub nanos: u32,
        }

        #[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
        #[serde(rename_all = "snake_case")]
        pub enum Level {
            Error,
            Warn,
            Info,
            Debug,
            Trace,
        }

        #[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
        #[serde(untagged)]
        pub enum LogValue {
            Bool(bool),
            I64(i64),
            U64(u64),
            F64(#[serde(deserialize_with = "crate::deserialize_finite_f64")] f64),
            String(String),
        }

        #[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
        pub struct Event {
            pub seq: u64,
            pub time: Timestamp,
            pub level: Level,
            pub target: String,
            pub message: String,
            pub fields: ::std::collections::BTreeMap<String, LogValue>,
            pub dropped: u32,
            #[serde(default)]
            pub truncated: u32,
        }

        #[cfg(test)]
        mod tests {
            use super::LogValue;

            #[test]
            fn non_finite_floats_are_rejected_on_decode() {
                let encoded = rmp_serde::to_vec_named(&LogValue::F64(f64::NAN))
                    .expect("messagepack permits the hostile input");
                assert!(rmp_serde::from_slice::<LogValue>(&encoded).is_err());
            }
        }
    }

    pub mod runtime {
        use crate::{Clock, RobotId};
        use phoxal_runtime_contract::version::RobotApiVersion;

        /// Empty request for the immutable facts of one running bundle.
        #[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        pub struct InfoRequest {}

        /// Runtime facts a supervisor advertises before an external client
        /// selects an exact robot-API adapter.
        #[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        pub struct ManualDrive {
            pub wheel_base_m: f64,
            pub max_linear_speed_mps: f64,
            pub max_angular_speed_radps: f64,
        }

        #[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        pub struct Info {
            pub robot: RobotId,
            pub clock: Clock,
            pub robot_api: RobotApiVersion,
            pub manual_drive: Option<ManualDrive>,
        }

        #[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
        pub struct Cursor {
            pub sequence: u64,
        }

        #[derive(
            Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, serde::Serialize, serde::Deserialize,
        )]
        #[serde(rename_all = "snake_case")]
        pub enum Direction {
            Publish,
            Subscribe,
            Mixed,
        }

        #[derive(
            Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, serde::Serialize, serde::Deserialize,
        )]
        #[serde(rename_all = "snake_case")]
        pub enum BufferKind {
            Outbound,
            Latest,
            Subscriber,
            Mixed,
        }

        #[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
        pub struct Step {
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

        #[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
        pub struct Topic {
            pub topic: String,
            pub direction: Direction,
            pub buffer_kind: BufferKind,
            pub count: u64,
            pub rate_millihz: u64,
            pub drops: u64,
            pub latest_overwrites: u64,
            pub bounded_evictions: u64,
            pub capacity: u64,
            pub current_depth: u64,
            pub high_water_depth: u64,
            pub decode_errors: u64,
            pub timeline_filtered: u64,
            pub overflowed_rows: u32,
        }

        #[cfg(test)]
        mod tests {
            use super::*;

            #[test]
            fn info_preserves_an_unknown_robot_api_for_client_dispatch() {
                let value = Info {
                    robot: RobotId::new("rover").expect("robot id"),
                    clock: Clock::Real,
                    robot_api: RobotApiVersion::new(42, 7),
                    manual_drive: None,
                };
                let encoded = rmp_serde::to_vec_named(&value).expect("info encodes");
                let decoded: Info = rmp_serde::from_slice(&encoded).expect("info decodes");
                assert_eq!(decoded.robot_api, RobotApiVersion::new(42, 7));
            }
        }
    }

    pub mod telemetry {
        #[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
        pub struct Rollup {
            pub window_ns: u64,
            pub step: Option<crate::payload::runtime::Step>,
            pub topics: Vec<crate::payload::runtime::Topic>,
            pub overflow: Option<crate::payload::runtime::Topic>,
        }

        #[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
        pub struct SnapshotRequest {
            pub participant_id: Option<String>,
            pub limit: u32,
            pub before_sequence: Option<u64>,
        }

        #[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
        pub struct Record {
            pub sequence: u64,
            pub participant_id: String,
            pub truncated: u32,
            pub window_ns: u64,
            pub step: Option<crate::payload::runtime::Step>,
            pub topics: Vec<crate::payload::runtime::Topic>,
            pub overflow: Option<crate::payload::runtime::Topic>,
        }

        #[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
        pub struct Snapshot {
            pub cursor: crate::payload::runtime::Cursor,
            pub records: Vec<Record>,
            pub capacity_evictions: u64,
            pub next_before_sequence: Option<u64>,
        }

        #[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
        pub struct Follow {
            pub cursor: crate::payload::runtime::Cursor,
            pub record: Record,
        }
    }

    pub mod log {
        #[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
        pub struct SnapshotRequest {
            pub participant_id: Option<String>,
            pub limit: u32,
            pub before_sequence: Option<u64>,
        }

        /// Logs use the same timestamp, severity and finite-value vocabulary
        /// at ingestion and when the supervisor later serves retained records.
        pub use crate::payload::logs::{Level, LogValue, Timestamp};

        #[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
        pub struct Record {
            pub sequence: u64,
            pub participant_id: String,
            pub source_sequence: u64,
            pub time: Timestamp,
            pub level: Level,
            pub target: String,
            pub message: String,
            pub fields: ::std::collections::BTreeMap<String, LogValue>,
            pub dropped: u32,
            pub truncated: u32,
        }

        #[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
        pub struct Snapshot {
            pub cursor: crate::payload::runtime::Cursor,
            pub ingest_dropped: u64,
            pub records: Vec<Record>,
            pub next_before_sequence: Option<u64>,
        }

        #[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
        pub struct Follow {
            pub cursor: crate::payload::runtime::Cursor,
            pub ingest_dropped: u64,
            pub record: Record,
        }
    }
}

phoxal_protocol! {
    protocol supervisor {
        logs(participant_id) {
            topic self: Stream<crate::payload::logs::Event>;
        }

        runtime {
            query info: crate::payload::runtime::InfoRequest => crate::payload::runtime::Info;
        }

        snapshot {
            topic self: Stream<crate::payload::snapshot::Update>;
            query current: crate::payload::snapshot::CurrentRequest => crate::payload::snapshot::Current;
        }

        control {
            query self: crate::payload::command::Request => crate::payload::command::Reply;
        }

        telemetry {
            topic rollup: Stream<crate::payload::telemetry::Rollup>;
            query snapshot: crate::payload::telemetry::SnapshotRequest => crate::payload::telemetry::Snapshot;
            topic follow: Stream<crate::payload::telemetry::Follow>;
        }

        log {
            query snapshot: crate::payload::log::SnapshotRequest => crate::payload::log::Snapshot;
            topic follow: Stream<crate::payload::log::Follow>;
        }

    }
}

#[cfg(test)]
mod tests {
    use phoxal_bus::{EndpointDescriptor, EndpointKind};

    use crate::supervisor;

    #[test]
    fn process_topics_are_protocol_qualified_not_robot_api_qualified() {
        assert_eq!(
            <supervisor::endpoint::telemetry::RollupEndpoint as EndpointDescriptor>::TOPIC,
            "supervisor/telemetry/rollup"
        );
        assert_eq!(
            <supervisor::endpoint::runtime::InfoEndpoint as EndpointDescriptor>::TOPIC,
            "supervisor/runtime/info"
        );
        assert_eq!(
            <supervisor::endpoint::snapshot::TopicEndpoint as EndpointDescriptor>::TOPIC,
            "supervisor/snapshot"
        );
        assert_eq!(
            <supervisor::endpoint::control::TopicEndpoint as EndpointDescriptor>::TOPIC,
            "supervisor/control"
        );
        assert_eq!(
            <supervisor::endpoint::telemetry::RollupEndpoint as EndpointDescriptor>::KIND,
            EndpointKind::Stream
        );
        assert_eq!(
            <supervisor::endpoint::snapshot::TopicEndpoint as EndpointDescriptor>::KIND,
            EndpointKind::Stream
        );
        assert_eq!(
            <supervisor::endpoint::snapshot::CurrentEndpoint as EndpointDescriptor>::TOPIC,
            "supervisor/snapshot/current"
        );
        assert_eq!(
            <supervisor::endpoint::control::TopicEndpoint as EndpointDescriptor>::KIND,
            EndpointKind::Query
        );
        assert!(
            crate::API_CONTRACT_MANIFEST
                .iter()
                .flat_map(|version| version.contracts)
                .all(|contract| contract.topic != crate::PRESENCE_KEY),
            "the presence lease must not collide with any protocol endpoint"
        );
    }
}
