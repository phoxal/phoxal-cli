//! The served protocol proven against the attachment client used by `phoxal`.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use phoxal_api::supervisor;
use phoxal_api::supervisor::bundle::GetResponse;
use phoxal_api::supervisor::command::{Command, CommandOutcome, CommandRejection};
use phoxal_api::supervisor::connect::{ConnectReply, PRESENCE_KEY};
use phoxal_api::supervisor::snapshot::Lifecycle;
use phoxal_bundle::{
    AssetIndex, BinaryReference, BinarySource, BundlePath, BundleWriter, ParticipantClock, Runtime,
    RuntimeBundle, RuntimeDocument, RuntimeParticipant,
};
use phoxal_bus::{
    BusConfig, BusOwner, Codec, DEFAULT_QUERY_TIMEOUT, EndpointDescriptor, MessagePack, Querier,
    SourceLabel,
};
use phoxal_client::{AttachError, Attachment, AttachmentConfig};
use phoxal_model::RobotBuilder;
use phoxal_runtime_contract::identity::{
    ExecutionId, ParticipantArtifactId, ParticipantId, ProducerId,
};
use phoxal_runtime_contract::metadata::{ParticipantContract, ParticipantKind as ContractKind};
use phoxal_runtime_contract::version::FrameworkVersion;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::{Control, serve};
use crate::daemon::projection::ExecutionFacts;
use crate::daemon::roster::Roster;
use crate::daemon::state::ExecutionState;
use crate::model::{ParticipantKind, ProcessState, StartupRequirement};
use crate::state::store::SupervisorState;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_real_attachment_observes_state_queries_diagnostics_and_stops() {
    let socket = tempfile::Builder::new()
        .prefix("phoxal-serve-")
        .tempdir_in("/tmp")
        .expect("short socket directory");
    let endpoint = format!("unixsock-stream/{}", socket.path().join("s.sock").display());
    let execution = ExecutionId::mint();
    let router = crate::router::start_embedded_router(
        execution,
        endpoint.clone(),
        None,
        Arc::new(|_| {}) as crate::router::RouterLost,
    )
    .await
    .expect("embedded router");
    let label = SourceLabel::new("phoxald-test").expect("source label");
    let (owner, bus) = BusOwner::open(BusConfig::for_external(
        execution,
        Some(label),
        vec![endpoint.clone()],
    ))
    .await
    .expect("supervisor bus");
    let identity = owner
        .declare_liveliness_key(PRESENCE_KEY)
        .await
        .expect("supervisor presence");

    let (_bundle_root, bundle) = bundle();
    let roster = Roster::from_bundle(&bundle);
    let board = SupervisorState::new();
    let state = ExecutionState::new(board.clone(), ExecutionFacts { roster });
    let key: crate::model::ProcessKey = ParticipantId::new("brain").expect("participant").into();
    board.upsert_process(
        key.clone(),
        ParticipantKind::Brain,
        ProcessState::Starting,
        StartupRequirement::Required,
    );
    let stop = CancellationToken::new();
    let (actions, _action_rx) = mpsc::channel(4);
    let served = tokio::spawn(serve(
        bus,
        state,
        Control {
            actions,
            stop: stop.clone(),
        },
        bundle,
        stop.clone(),
    ));

    let attachment = attach(&endpoint).await;
    assert_eq!(attachment.execution(), execution);
    assert_eq!(attachment.connected().robot.as_str(), "rover");
    assert_eq!(
        attachment
            .port()
            .snapshot()
            .expect("initial snapshot")
            .lifecycle,
        Lifecycle::Starting
    );
    assert!(
        attachment
            .port()
            .logs(None, 10, None)
            .await
            .expect("log snapshot")
            .records
            .is_empty()
    );
    assert!(
        attachment
            .port()
            .telemetry(None, 10, None)
            .await
            .expect("telemetry snapshot")
            .records
            .is_empty()
    );

    let mut snapshots = attachment.port().snapshots();
    board.record_instance_presence(
        ParticipantId::new("brain").expect("participant"),
        ProducerId::try_from(1_u128 << 124).expect("producer"),
        true,
    );
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            snapshots.changed().await.expect("snapshot stream");
            if snapshots
                .borrow_and_update()
                .as_ref()
                .is_some_and(|snapshot| {
                    snapshot.processes.iter().any(|process| {
                        process.participant.as_str() == "brain"
                            && process.state
                                == phoxal_api::supervisor::snapshot::ProcessState::Ready
                    })
                })
            {
                break;
            }
        }
    })
    .await
    .expect("state publication");

    let bus = attachment.port().bus().clone();

    // The bundle endpoint answers out of the daemon's own root: a file it
    // holds, a name it does not, and the three shapes of path it refuses to
    // resolve at all.
    let files = Querier::new(
        bus.clone(),
        &supervisor::topic::client().bundle().get(),
        DEFAULT_QUERY_TIMEOUT,
    )
    .expect("bundle querier");
    let found = bundle_entry(&files, "runtime.json").await;
    assert!(
        matches!(&found, GetResponse::Found { bytes } if !bytes.is_empty()),
        "{found:?}"
    );
    for (path, expected) in [
        ("assets/nothing-here", GetResponse::Missing),
        ("../outside", GetResponse::InvalidPath),
        ("/etc/passwd", GetResponse::InvalidPath),
        ("", GetResponse::InvalidPath),
    ] {
        assert_eq!(bundle_entry(&files, path).await, expected, "path {path:?}");
    }

    // Stop is a compare-and-swap on the snapshot revision, and the two host
    // operations are not this supervisor's to perform at all.
    let commands = Querier::new(
        bus,
        &supervisor::topic::client().command().topic(),
        DEFAULT_QUERY_TIMEOUT,
    )
    .expect("command querier");
    for (command, reason) in [
        (
            Command::Stop {
                expected_revision: u64::MAX,
            },
            CommandRejection::RevisionStale,
        ),
        (
            Command::Reboot {
                expected_revision: 0,
            },
            CommandRejection::UnsupportedHostAction,
        ),
        (
            Command::Poweroff {
                expected_revision: 0,
            },
            CommandRejection::UnsupportedHostAction,
        ),
    ] {
        assert_eq!(
            issue(&commands, command.clone()).await,
            CommandOutcome::Rejected { reason },
            "{command:?}"
        );
    }

    // The port fences on the revision it has actually installed, so the same
    // command the operator issues is the one that is accepted.
    assert!(matches!(
        attachment.port().stop().await.expect("stop reply"),
        CommandOutcome::Accepted { .. }
    ));
    stop.cancelled().await;
    attachment.close().await.expect("attachment closes");
    served.await.expect("serve task joins").expect("serve task");
    drop(identity);
    assert!(owner.close().await.is_clean());
    router.close().await.expect("router closes");
}

async fn bundle_entry(
    querier: &Querier<supervisor::bundle::GetRequest, GetResponse>,
    path: &str,
) -> GetResponse {
    querier
        .query(supervisor::bundle::GetRequest {
            path: path.to_string(),
        })
        .await
        .expect("the bundle endpoint answers")
}

async fn issue(
    querier: &Querier<supervisor::command::Request, supervisor::command::Reply>,
    command: Command,
) -> CommandOutcome {
    let supervisor::command::Reply::V0 { outcome } = querier
        .query(supervisor::command::Request::V0 { command })
        .await
        .expect("the command endpoint answers");
    outcome
}

/// A client refuses a robot from another compatibility line at the bootstrap,
/// before it asks for anything else.
///
/// The stub answers `supervisor/connect` and nothing at all besides, so an
/// attachment that got past the gate could not have completed: the refusal is
/// the only way this can end.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_foreign_framework_line_is_refused_at_the_bootstrap() {
    let robot = FrameworkVersion::new(9, 9, 9);
    assert!(!robot.is_compatible_with(FrameworkVersion::CURRENT));
    let reply =
        MessagePack::encode(&ConnectReply::V0 { framework: robot }).expect("the reply encodes");
    let error = attach_to_bootstrap_stub(reply, |error, _| error.is_framework_mismatch()).await;
    let AttachError::IncompatibleFramework {
        robot: reported,
        client,
    } = &error
    else {
        panic!("a foreign line must be reported as one: {error}");
    };
    assert_eq!(*reported, robot);
    assert_eq!(*client, FrameworkVersion::CURRENT);
    let rendered = error.to_string();
    assert!(rendered.contains("Robot framework: 9.9.9"), "{rendered}");
    assert!(rendered.contains("phoxal self upgrade"), "{rendered}");
}

/// A robot on another train of this client's own line is admitted by the
/// bootstrap gate: the attachment goes on to the ordinary endpoints, which the
/// stub does not serve, and fails there instead of as a contract disagreement.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn another_train_on_this_line_is_admitted_at_the_bootstrap() {
    let client = FrameworkVersion::CURRENT;
    let robot = FrameworkVersion::new(
        client.major(),
        client.minor(),
        client.patch().wrapping_add(1),
    );
    assert_ne!(robot, client);
    let reply =
        MessagePack::encode(&ConnectReply::V0 { framework: robot }).expect("the reply encodes");
    let error = attach_to_bootstrap_stub(reply, |_, answered| answered > 0).await;
    assert!(
        !error.is_framework_mismatch(),
        "a train on this line must not be refused as a contract disagreement: {error}"
    );
}

/// A bootstrap reply this client cannot decode is the same answer by another
/// route, and the schema tag the robot sent survives into the diagnostic.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unreadable_bootstrap_reply_names_the_foreign_schema_tag() {
    const FOREIGN: &str = "phoxal/supervisor-connect/v1";
    let reply = MessagePack::encode(&serde_json::json!({
        "schema": FOREIGN,
        "framework": "9.9.9",
    }))
    .expect("the foreign reply encodes");
    let error = attach_to_bootstrap_stub(reply, |error, _| error.is_framework_mismatch()).await;
    let AttachError::UnreadableConnectReply { detail } = &error else {
        panic!("an unreadable bootstrap must be reported as a contract mismatch: {error}");
    };
    assert!(detail.contains(FOREIGN), "{detail}");
}

/// Drive the real attachment gate against a supervisor that answers the frozen
/// bootstrap with exactly `reply` and serves no other endpoint.
///
/// It exists because no two binaries built from this workspace can disagree
/// about the train: the peer the gate is written for has to be stood up
/// deliberately.
///
/// Attaching is retried until `settled` accepts the failure, because the
/// embedded router and the stub server come up asynchronously and the early
/// attempts see neither. `settled` is given the failure and how many bootstrap
/// queries the stub has answered by then, so a caller that expects the gate to
/// *admit* the peer can tell "the bootstrap completed and the attachment went
/// on" apart from "the bootstrap was never reached".
async fn attach_to_bootstrap_stub(
    reply: Vec<u8>,
    settled: impl Fn(&AttachError, usize) -> bool,
) -> AttachError {
    let socket = tempfile::Builder::new()
        .prefix("phoxal-connect-")
        .tempdir_in("/tmp")
        .expect("short socket directory");
    let endpoint = format!("unixsock-stream/{}", socket.path().join("s.sock").display());
    let execution = ExecutionId::mint();
    let router = crate::router::start_embedded_router(
        execution,
        endpoint.clone(),
        None,
        Arc::new(|_| {}) as crate::router::RouterLost,
    )
    .await
    .expect("embedded router");
    let (owner, bus) = BusOwner::open(BusConfig::for_external(
        execution,
        Some(SourceLabel::new("phoxald-stub").expect("source label")),
        vec![endpoint.clone()],
    ))
    .await
    .expect("stub supervisor bus");

    let answered = Arc::new(AtomicUsize::new(0));
    let server_bus = bus.clone();
    let counter = Arc::clone(&answered);
    let served = tokio::spawn(async move {
        let server = server_bus
            .declare_server(
                <supervisor::endpoint::connect::TopicEndpoint as EndpointDescriptor>::TOPIC,
            )
            .await
            .expect("bootstrap server");
        loop {
            let incoming = server.recv().await.expect("a bootstrap query");
            incoming
                .reply(&server_bus, reply.clone())
                .await
                .expect("the stub answers");
            counter.fetch_add(1, Ordering::SeqCst);
        }
    });

    let config = AttachmentConfig::new(&endpoint, "test-client");
    let mut outcome = None;
    for _ in 0..100 {
        match Attachment::open(&config).await {
            Ok(_) => panic!("a stub that serves only the bootstrap can never be attached to"),
            Err(error) if settled(&error, answered.load(Ordering::SeqCst)) => {
                outcome = Some(error);
                break;
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
        }
    }

    served.abort();
    assert!(owner.close().await.is_clean());
    router.close().await.expect("router closes");
    outcome.expect("the stub bootstrap answered and the gate reached its verdict")
}

async fn attach(endpoint: &str) -> Attachment {
    let config = AttachmentConfig::new(endpoint, "test-client");
    let mut last = None;
    for _ in 0..100 {
        match Attachment::open(&config).await {
            Ok(attachment) => return attachment,
            Err(error) => {
                last = Some(error);
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    }
    panic!("a client never attached: {last:?}");
}

fn bundle() -> (tempfile::TempDir, RuntimeBundle) {
    let root = tempfile::tempdir().expect("temporary bundle parent");
    let source = BinarySource::open(std::env::current_exe().expect("test executable"))
        .expect("test executable source");
    let artifact_id = ParticipantArtifactId::new("brain").expect("artifact id");
    let binary_path = BundlePath::new("bin/brain").expect("binary path");
    let reference = BinaryReference::from_source(
        binary_path.clone(),
        ParticipantContract {
            framework: FrameworkVersion::CURRENT,
            id: artifact_id.clone(),
            kind: ContractKind::Brain,
            requirement: None,
            config_schema: serde_json::json!({"type": "null"}),
        },
        &source,
    )
    .expect("binary reference");
    let runtime = Runtime::new(
        RobotBuilder::new("rover").build().expect("robot"),
        BTreeMap::from([(artifact_id.clone(), reference)]),
        vec![RuntimeParticipant::new(
            ParticipantId::new("brain").expect("participant"),
            artifact_id,
            None,
            None,
            ParticipantClock::Real,
        )],
        AssetIndex::from_bytes(&BTreeMap::new()).expect("asset index"),
        None,
    )
    .expect("runtime");
    let bundle = BundleWriter::write(
        root.path().join("bundle"),
        &RuntimeDocument::new(runtime),
        &BTreeMap::new(),
        &BTreeMap::from([(binary_path, source)]),
    )
    .expect("bundle");
    (root, bundle)
}
