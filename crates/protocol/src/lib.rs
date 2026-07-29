//! Bounded wire protocol for the project-local resident supervisor.
//!
//! This crate owns wire DTOs, frame limits, pure codecs, and thin I/O
//! adapters. It owns no sockets, reconnect loops, resident state, or UI.

pub mod codec;
mod command;
mod handshake;
pub mod limits;
mod snapshot;

pub use command::{
    BootstrapResult, CommandAction, CommandError, CommandKey, CommandReply, CommandRequest,
    CommandSessionId,
};
pub use handshake::{
    ConnectionRole, HandshakeReply, HandshakeRequest, SUPERVISOR_PROTOCOL_VERSION,
};
pub use snapshot::{SupervisorSnapshot, SupervisorSnapshotV0, validate_snapshot_bounds};

#[cfg(test)]
mod tests {
    use std::io::Cursor;
    use std::time::Duration;

    use phoxal_cli_core::identity::{ExecutionId, ProducerId};
    use phoxal_cli_core::runtime::{
        BoundedString, DesiredProcessState, ParticipantKind, ProcessDescriptor, ProcessEntry,
        ProcessFailure, ProcessFailureKind, ProcessKey, ProcessState, ProcessStatus,
        ProjectLifecycle, RobotKey, SimulationSessionInfo, StartupRequirement, StartupStatus,
    };

    use super::*;

    fn sample_snapshot() -> SupervisorSnapshot {
        SupervisorSnapshot::V0(SupervisorSnapshotV0 {
            supervisor_generation: 42,
            revision: 7,
            execution_id: ExecutionId::parse(&"1".repeat(ExecutionId::LEN)).unwrap(),
            project: "/path/to/project".to_string(),
            entry: "/path/to/project/robot.yaml".to_string(),
            framework_train: "0.42.0".to_string(),
            simulation: None,
            lifecycle: ProjectLifecycle::Ready,
            router: "unixsock-stream//tmp/router.sock".to_string(),
            plan_revision: 1,
            graph_generation: 0,
            startup: phoxal_cli_core::runtime::StartupStatus {
                completed_phases: Vec::new(),
                active_phase: None,
            },
            processes: std::collections::BTreeMap::new(),
            failure: None,
        })
    }

    #[test]
    fn snapshot_has_exact_tagged_json_shape_and_dispatches() {
        let snapshot = sample_snapshot();
        let value = serde_json::to_value(&snapshot).unwrap();
        assert_eq!(
            value,
            serde_json::json!({
                "schema": "phoxal/supervisor-snapshot/v0",
                "supervisor_generation": 42,
                "revision": 7,
                "execution_id": "11111111111111111111111111111111",
                "project": "/path/to/project",
                "entry": "/path/to/project/robot.yaml",
                "framework_train": "0.42.0",
                "simulation": null,
                "lifecycle": "ready",
                "router": "unixsock-stream//tmp/router.sock",
                "plan_revision": 1,
                "graph_generation": 0,
                "startup": {
                    "completed_phases": [],
                    "active_phase": null
                },
                "processes": {},
                "failure": null
            })
        );
        let decoded: SupervisorSnapshot = serde_json::from_value(value).unwrap();
        assert_eq!(decoded.as_v0().revision, 7);
        assert_eq!(decoded.into_v0().supervisor_generation, 42);
    }

    #[test]
    fn snapshot_rejects_unknown_schema_and_fields() {
        let mut value = serde_json::to_value(sample_snapshot()).unwrap();
        value["schema"] = serde_json::json!("phoxal/supervisor-snapshot/v1");
        assert!(serde_json::from_value::<SupervisorSnapshot>(value).is_err());

        let mut value = serde_json::to_value(sample_snapshot()).unwrap();
        value["legacy_execution"] = serde_json::json!("simulation:webots");
        assert!(serde_json::from_value::<SupervisorSnapshot>(value).is_err());
    }

    #[test]
    fn failure_reason_beyond_the_bound_is_rejected() {
        let mut snapshot = sample_snapshot().into_v0();
        snapshot.failure = Some("f".repeat(limits::MAX_SUPERVISOR_FAILURE_REASON_BYTES + 1));
        assert!(validate_snapshot_bounds(&snapshot).is_err());
    }

    #[test]
    fn simulation_information_has_exact_wire_shape() {
        let value = serde_json::to_value(SimulationSessionInfo {
            profile: "webots".to_string(),
            world: "worlds/default.wbt".to_string(),
        })
        .unwrap();
        assert_eq!(
            value,
            serde_json::json!({
                "profile": "webots",
                "world": "worlds/default.wbt",
            })
        );
    }

    #[test]
    fn pure_codec_enforces_payload_and_frame_bounds() {
        let reply = HandshakeReply {
            protocol_version: 1,
            supervisor_generation: 9,
            command_session: None,
        };
        let frame = codec::encode_frame(&reply, limits::MAX_HANDSHAKE_FRAME_BYTES).unwrap();
        assert_eq!(
            codec::decode_frame::<HandshakeReply>(&frame, limits::MAX_HANDSHAKE_FRAME_BYTES)
                .unwrap(),
            reply
        );
        assert!(codec::encode_frame(&reply, 1).is_err());

        let mut oversized = frame;
        oversized[..4].copy_from_slice(
            &(u32::try_from(limits::MAX_HANDSHAKE_FRAME_BYTES + 1).unwrap()).to_be_bytes(),
        );
        let error =
            codec::decode_frame::<HandshakeReply>(&oversized, limits::MAX_HANDSHAKE_FRAME_BYTES)
                .unwrap_err();
        assert!(error.to_string().contains("protocol frame declares"));
    }

    #[test]
    fn reference_graph_process_count_remains_bounded() {
        limits::validate_snapshot_capacity(limits::MAX_SUPERVISED_PROCESSES).unwrap();
        assert!(limits::validate_snapshot_capacity(limits::MAX_SUPERVISED_PROCESSES + 1).is_err());
        assert!(
            limits::worst_case_snapshot_bytes(limits::MAX_SUPERVISED_PROCESSES).unwrap()
                <= limits::MAX_SNAPSHOT_FRAME_BYTES
        );
    }

    #[test]
    fn maximum_terminal_graph_serializes_below_v0_frame_ceiling() {
        let mut processes = std::collections::BTreeMap::new();
        for index in 0..limits::MAX_SUPERVISED_PROCESSES {
            let key = ProcessKey::robot(
                RobotKey::new(
                    "n".repeat(limits::MAX_SNAPSHOT_TEXT_BYTES),
                    "r".repeat(limits::MAX_SNAPSHOT_TEXT_BYTES),
                ),
                format!(
                    "p{index:02}{}",
                    "x".repeat(limits::MAX_ARTIFACT_ID_BYTES - 3)
                ),
            );
            processes.insert(
                key.clone(),
                ProcessEntry {
                    descriptor: ProcessDescriptor {
                        key,
                        kind: ParticipantKind::Service,
                        artifact: "a".repeat(limits::MAX_ARTIFACT_ID_BYTES),
                        owner: "o".repeat(limits::MAX_SNAPSHOT_TEXT_BYTES),
                        startup_requirement: StartupRequirement::Required,
                    },
                    status: ProcessStatus {
                        desired: DesiredProcessState::Running,
                        actual: ProcessState::Failed,
                        pid: Some(u32::MAX),
                        producer: Some(ProducerId::mint()),
                        restart_count_in_generation: u32::MAX,
                        restart_count_total: u64::MAX,
                        last_failure: Some(ProcessFailure {
                            kind: ProcessFailureKind::Exit,
                            occurred_at: std::time::SystemTime::UNIX_EPOCH,
                            exit: None,
                            detail: BoundedString::with_max_bytes(
                                "d".repeat(limits::MAX_PROCESS_FAILURE_DETAIL_BYTES),
                                limits::MAX_PROCESS_FAILURE_DETAIL_BYTES,
                            ),
                            stderr_tail: Some(BoundedString::with_max_bytes(
                                "e".repeat(limits::MAX_PROCESS_STDERR_TAIL_BYTES),
                                limits::MAX_PROCESS_STDERR_TAIL_BYTES,
                            )),
                        }),
                    },
                },
            );
        }
        let snapshot = SupervisorSnapshotV0 {
            supervisor_generation: u64::MAX,
            revision: u64::MAX,
            execution_id: ExecutionId::mint(),
            project: "p".repeat(limits::MAX_SNAPSHOT_TEXT_BYTES),
            entry: "e".repeat(limits::MAX_SNAPSHOT_TEXT_BYTES),
            framework_train: "t".repeat(limits::MAX_SNAPSHOT_TEXT_BYTES),
            simulation: None,
            lifecycle: ProjectLifecycle::Failed,
            router: "r".repeat(limits::MAX_SNAPSHOT_TEXT_BYTES),
            plan_revision: u64::MAX,
            graph_generation: u64::MAX,
            startup: StartupStatus {
                completed_phases: (0..limits::MAX_STARTUP_PHASES)
                    .map(|_| "s".repeat(limits::MAX_SNAPSHOT_TEXT_BYTES))
                    .collect(),
                active_phase: Some("s".repeat(limits::MAX_SNAPSHOT_TEXT_BYTES)),
            },
            processes,
            failure: Some("f".repeat(limits::MAX_SUPERVISOR_FAILURE_REASON_BYTES)),
        };
        validate_snapshot_bounds(&snapshot).unwrap();
        let frame = codec::encode_frame(
            &SupervisorSnapshot::V0(snapshot.clone()),
            limits::MAX_SNAPSHOT_FRAME_BYTES,
        )
        .unwrap();
        assert!(frame.len() - 4 <= limits::MAX_SNAPSHOT_FRAME_BYTES);

        let mut too_many = snapshot;
        let key = ProcessKey::project("overflow");
        too_many.processes.insert(
            key.clone(),
            ProcessEntry {
                descriptor: ProcessDescriptor {
                    key,
                    kind: ParticipantKind::Service,
                    artifact: "overflow".to_string(),
                    owner: "phoxal-cli".to_string(),
                    startup_requirement: StartupRequirement::Required,
                },
                status: ProcessStatus::default(),
            },
        );
        assert!(validate_snapshot_bounds(&too_many).is_err());
    }

    #[tokio::test]
    async fn async_and_blocking_adapters_emit_and_decode_identical_frames() {
        let value = sample_snapshot();
        let maximum = limits::MAX_SNAPSHOT_FRAME_BYTES;

        let mut blocking = Cursor::new(Vec::new());
        codec::blocking_io::write_frame(&mut blocking, &value, maximum).unwrap();
        let blocking_bytes = blocking.into_inner();

        let (mut writer, mut reader) = tokio::io::duplex(blocking_bytes.len() * 2);
        codec::async_io::write_frame(&mut writer, &value, maximum, Duration::from_secs(1))
            .await
            .unwrap();
        let mut async_bytes = vec![0; blocking_bytes.len()];
        tokio::io::AsyncReadExt::read_exact(&mut reader, &mut async_bytes)
            .await
            .unwrap();
        assert_eq!(async_bytes, blocking_bytes);

        let mut blocking = Cursor::new(async_bytes.clone());
        let blocking_value: SupervisorSnapshot =
            codec::blocking_io::read_frame(&mut blocking, maximum).unwrap();
        let mut async_reader = Cursor::new(async_bytes);
        let async_value: SupervisorSnapshot =
            codec::async_io::read_frame(&mut async_reader, maximum, Duration::from_secs(1))
                .await
                .unwrap();
        assert_eq!(async_value, blocking_value);
    }

    #[tokio::test]
    async fn both_adapters_reject_an_oversized_declared_frame_before_allocating() {
        let maximum = limits::MAX_HANDSHAKE_FRAME_BYTES;
        let header = (u32::try_from(maximum + 1).unwrap()).to_be_bytes();

        let mut blocking = Cursor::new(header);
        let blocking_error =
            codec::blocking_io::read_frame::<_, HandshakeReply>(&mut blocking, maximum)
                .unwrap_err();
        assert!(
            blocking_error
                .to_string()
                .contains("protocol frame declares")
        );

        let mut asynchronous = Cursor::new(header);
        let async_error = codec::async_io::read_frame::<_, HandshakeReply>(
            &mut asynchronous,
            maximum,
            Duration::from_secs(1),
        )
        .await
        .unwrap_err();
        assert!(async_error.to_string().contains("protocol frame declares"));
    }
}
