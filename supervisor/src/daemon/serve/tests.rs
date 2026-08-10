//! The served protocol proven against the attachment client used by `phoxal`.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use phoxal_bundle::{
    AssetIndex, BinaryReference, BinarySource, BundlePath, BundleWriter, ParticipantClock, Runtime,
    RuntimeBundle, RuntimeDocument, RuntimeParticipant,
};
use phoxal_bus::{BusConfig, BusOwner, SourceLabel};
use phoxal_model::RobotBuilder;
use phoxal_runtime_contract::identity::{
    ExecutionId, ParticipantArtifactId, ParticipantId, ProducerId,
};
use phoxal_runtime_contract::metadata::{
    ParticipantContract, ParticipantKind as ContractKind, ParticipantSchemas,
};
use phoxal_runtime_contract::version::{BusAbi, LaunchAbi, RobotApiVersion, RuntimeSchema};
use phoxal_supervisor_api::{CommandOutcome, Lifecycle, PRESENCE_KEY};
use phoxal_supervisor_client::{Attachment, AttachmentConfig};
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
                            && process.state == phoxal_supervisor_api::ProcessState::Ready
                    })
                })
            {
                break;
            }
        }
    })
    .await
    .expect("state publication");

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
            id: artifact_id.clone(),
            kind: ContractKind::Brain,
            api: RobotApiVersion::new(0, 1),
            schemas: ParticipantSchemas {
                bus: BusAbi::V0,
                launch: LaunchAbi::V0,
                runtime: RuntimeSchema::V0,
            },
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
