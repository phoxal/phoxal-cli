//! The served contract, proven against a real attachment.
//!
//! These are the only tests in this crate that stand a whole supervisor up: an
//! embedded router on a real endpoint, this daemon's serving session on it, and
//! `phoxal-supervisor-client` attaching the way `phoxal attach` does. Everything
//! else about the endpoints is decided by pure functions with their own tests;
//! what needs a live fabric is precisely the part those cannot cover - that the
//! keys, brands, and codecs on both sides actually meet.

use std::path::Path;
use std::sync::Arc;

use phoxal_bus::{Bus, BusConfig};
use phoxal_cli_core::identity::{ExecutionId, ProducerId};
use phoxal_cli_core::runtime::{ParticipantKind, ProcessState, StartupRequirement};
use phoxal_manifest::bundle::BundleResolver;
use phoxal_supervisor_api::{
    BundleGetOutcome, Command, CommandOutcome, CommandRejection, ExecutionMode, Name,
    ProcessKey as WireProcessKey, RobotIdentity,
};
use phoxal_supervisor_client::{Attachment, AttachmentConfig};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::command::Control;
use crate::SupervisorState;
use crate::daemon::projection::ExecutionFacts;
use crate::daemon::roster::tests::roster;
use crate::daemon::state::ExecutionState;

/// A supervisor standing on a real router, with everything an attachment
/// touches wired the way `phoxald` wires it.
struct Fixture {
    endpoint: String,
    execution: ExecutionId,
    state: ExecutionState,
    stop: CancellationToken,
    commands: mpsc::Receiver<crate::SupervisorAction>,
    _bundle: tempfile::TempDir,
    _router: crate::EmbeddedRouter,
    _socket: tempfile::TempDir,
    _served: tokio::task::JoinHandle<anyhow::Result<()>>,
    _bridge: tokio::task::JoinHandle<()>,
}

impl Fixture {
    async fn start() -> Self {
        let socket = tempfile::Builder::new()
            .prefix("phoxal-serve-")
            // A unix socket path is short by necessity.
            .tempdir_in("/tmp")
            .expect("short-path temp dir");
        let endpoint = format!("unixsock-stream/{}", socket.path().join("s.sock").display());
        let execution = ExecutionId::mint();
        let router = crate::start_embedded_router(
            execution,
            endpoint.clone(),
            None,
            Arc::new(|_| {}) as crate::RouterLost,
        )
        .await
        .expect("the embedded router binds");

        let bundle = bundle();
        let board = SupervisorState::new();
        let state = ExecutionState::new(
            board.clone(),
            ExecutionFacts {
                robot: RobotIdentity {
                    id: Name::new("rover"),
                    namespace: Name::new("demo"),
                },
                mode: ExecutionMode::Simulated,
                roster: roster(),
            },
        );
        for entry in roster().entries() {
            board.upsert_process(
                entry.core.clone(),
                ParticipantKind::Service,
                ProcessState::Starting,
                StartupRequirement::Required,
            );
        }
        let bridge = state.bridge_store_changes();

        let bus = Bus::open(BusConfig {
            execution,
            participant: "phoxald".to_string(),
            connect_endpoints: vec![endpoint.clone()],
        })
        .await
        .expect("the supervisor session opens");
        let stop = CancellationToken::new();
        let (actions, commands) = mpsc::channel(8);
        let served = tokio::spawn(super::serve(
            bus,
            state.clone(),
            Control {
                actions,
                stop: stop.clone(),
            },
            BundleResolver::index(bundle.path(), 1024).expect("index the bundle"),
            stop.clone(),
        ));

        Self {
            endpoint,
            execution,
            state,
            stop,
            commands,
            _bundle: bundle,
            _router: router,
            _socket: socket,
            _served: served,
            _bridge: bridge,
        }
    }

    async fn attach(&self) -> Attachment {
        // Serving is asynchronous, so retry rather than assume the queryables
        // are already declared.
        let config = AttachmentConfig::new(&self.endpoint, "test-client");
        let mut last = None;
        for _ in 0..100 {
            match Attachment::open(&config).await {
                Ok(attachment) => return attachment,
                Err(error) => {
                    last = Some(error);
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }
            }
        }
        panic!("a client never attached: {last:?}");
    }
}

fn bundle() -> tempfile::TempDir {
    let root = tempfile::tempdir().expect("temp dir");
    std::fs::write(root.path().join("robot.yaml"), b"schema: phoxal/robot/v0\n")
        .expect("robot.yaml");
    std::fs::create_dir_all(root.path().join("bin")).expect("bin");
    std::fs::write(root.path().join("bin/brain"), b"elf").expect("brain");
    root
}

fn producer(seed: u8) -> ProducerId {
    ProducerId::try_from(u128::from(seed)).expect("a producer id")
}

/// The whole attachment handshake against the real server: the client reads the
/// execution off the router, decodes the connect reply, subscribes the snapshot
/// stream, and queries the current document. Every step is one this daemon has
/// to serve for `phoxal attach` to work at all.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_client_attaches_and_reads_the_execution_off_the_router() {
    let fixture = Fixture::start().await;
    let attachment = fixture.attach().await;

    assert_eq!(
        attachment.execution(),
        fixture.execution,
        "the execution a client learns IS the router's identity"
    );
    let connected = attachment.connected();
    assert_eq!(connected.robot.id.as_str(), "rover");
    assert_eq!(connected.robot.namespace.as_str(), "demo");
    assert_eq!(
        connected.mode,
        ExecutionMode::Simulated,
        "the clock field is passed through, not interpreted"
    );
    assert!(
        !attachment.is_disconnected(),
        "the identity token is live while the supervisor is"
    );

    let snapshot = attachment.snapshot().expect("the current query answered");
    assert_eq!(
        snapshot
            .processes
            .iter()
            .map(|process| process.key.to_string())
            .collect::<Vec<_>>(),
        ["brain", "service:drive", "driver:left", "simulator:webots"],
        "the snapshot reports the selected process set"
    );

    attachment.close().await.expect("detaching closes cleanly");
}

/// Every state change reaches an attached client, at a strictly higher
/// revision. This is the subscribe-then-query contract working end to end: the
/// client installed the current document at attach, and the stream carries it
/// forward from there.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_state_change_reaches_an_attached_client_at_a_higher_revision() {
    let fixture = Fixture::start().await;
    let attachment = fixture.attach().await;
    let installed = attachment
        .snapshot()
        .expect("an installed snapshot")
        .revision;

    let brain = roster()
        .resolve(&WireProcessKey::Brain)
        .expect("the brain is selected")
        .core
        .clone();
    fixture
        .state
        .board()
        .set_state(&brain, ProcessState::Ready, None);

    let mut snapshots = attachment.snapshots();
    let ready = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            snapshots.changed().await.expect("the stream stays open");
            let snapshot = snapshots.borrow_and_update().clone();
            if let Some(snapshot) = snapshot
                && snapshot.processes.iter().any(|process| {
                    process.key == WireProcessKey::Brain
                        && process.state == phoxal_supervisor_api::ProcessState::Ready
                })
            {
                return snapshot;
            }
        }
    })
    .await
    .expect("a published change reaches the client");
    assert!(
        ready.revision > installed,
        "{} must be newer than {installed}",
        ready.revision
    );

    attachment.close().await.expect("detach");
}

/// The fence, over the wire: a restart is accepted only when the producer the
/// client saw is the one the supervisor learned, and an accepted restart
/// actually reaches the supervision loop's queue.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_restart_is_fenced_on_the_producer_the_client_saw() {
    let mut fixture = Fixture::start().await;
    let attachment = fixture.attach().await;
    let brain = roster()
        .resolve(&WireProcessKey::Brain)
        .expect("the brain")
        .core
        .clone();

    // Nothing has opened a session, so the supervisor knows no producer and no
    // expectation can match.
    assert_eq!(
        attachment
            .restart(WireProcessKey::Brain, producer(7))
            .await
            .expect("the query answered"),
        CommandOutcome::Rejected {
            reason: CommandRejection::ProducerFenced
        }
    );
    assert!(fixture.commands.try_recv().is_err());

    fixture.state.board().set_producer(&brain, producer(7));
    assert_eq!(
        attachment
            .restart(WireProcessKey::Brain, producer(7))
            .await
            .expect("the query answered"),
        CommandOutcome::Accepted
    );
    assert!(
        matches!(
            fixture.commands.try_recv(),
            Ok(crate::SupervisorAction::Restart { key }) if key == brain
        ),
        "an accepted restart reaches the supervision loop"
    );

    // A process this execution never selected is rejected before the fence.
    assert_eq!(
        attachment
            .restart(
                WireProcessKey::Service {
                    id: Name::new("absent")
                },
                producer(7)
            )
            .await
            .expect("the query answered"),
        CommandOutcome::Rejected {
            reason: CommandRejection::UnknownProcess
        }
    );

    attachment.close().await.expect("detach");
}

/// `Stop` ends the execution rather than merely being acknowledged.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stop_cancels_the_execution() {
    let fixture = Fixture::start().await;
    let attachment = fixture.attach().await;
    assert!(!fixture.stop.is_cancelled());

    assert_eq!(
        attachment.command(Command::Stop).await.expect("answered"),
        CommandOutcome::Accepted
    );
    tokio::time::timeout(std::time::Duration::from_secs(5), fixture.stop.cancelled())
        .await
        .expect("an accepted stop cancels the execution");
}

/// The bundle is readable through the wire - manifest and binary alike - and a
/// traversal never is.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_client_reads_the_finalized_manifest_and_never_escapes_the_bundle() {
    let fixture = Fixture::start().await;
    let attachment = fixture.attach().await;

    let outcome = attachment
        .bundle_file("robot.yaml")
        .await
        .expect("the query answered");
    let BundleGetOutcome::Found { bytes } = outcome else {
        panic!("robot.yaml must be readable through bundle/get: {outcome:?}");
    };
    assert!(String::from_utf8_lossy(&bytes).contains("phoxal/robot/v0"));

    assert!(matches!(
        attachment
            .bundle_file("bin/brain")
            .await
            .expect("the query answered"),
        BundleGetOutcome::Found { .. }
    ));
    assert_eq!(
        attachment
            .bundle_file("assets/nothing")
            .await
            .expect("the query answered"),
        BundleGetOutcome::Missing
    );
    // The client refuses this locally with the same rules the supervisor
    // applies, so it never reaches the wire at all.
    assert!(attachment.bundle_file("../secret").await.is_err());

    attachment.close().await.expect("detach");
}

/// The log and telemetry endpoints answer an empty history rather than timing
/// out, which is what makes them safe for a client to query at attach time.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_log_and_telemetry_endpoints_answer_before_anything_has_been_collected() {
    let fixture = Fixture::start().await;
    let attachment = fixture.attach().await;

    let phoxal_supervisor_api::supervisor::logs::Snapshot::V0 {
        records,
        next_before_sequence,
        ..
    } = attachment.logs(None, 0, None).await.expect("logs answer");
    assert!(records.is_empty());
    assert_eq!(next_before_sequence, None);

    let phoxal_supervisor_api::supervisor::telemetry::Snapshot::V0 { records, .. } = attachment
        .telemetry(None, 0, None)
        .await
        .expect("telemetry answers");
    assert!(records.is_empty());

    // The follow streams declare, which is what a client does before paging.
    attachment.follow_logs().await.expect("follow logs");
    attachment
        .follow_telemetry()
        .await
        .expect("follow telemetry");

    attachment.close().await.expect("detach");
}

/// The daemon package may not depend on anything that would let it build,
/// render, or read input. This is a boundary the issue states and nothing else
/// enforces: a stray dependency here is how a durable systemd-owned daemon
/// quietly acquires Cargo.
#[test]
fn the_daemon_depends_on_nothing_that_builds_renders_or_reads_input() {
    let manifest =
        std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml"))
            .expect("read this package's manifest");
    for forbidden in [
        // Cargo, registries, source resolution, staging, deployment, and the
        // Webots application lifecycle all live here.
        "phoxal-cli-project",
        // Terminal presentation.
        "phoxal-cli-ui",
        // The disposable attached client, its input devices, and its joypad.
        "phoxal-cli-client",
        // Client-side projections of what this daemon publishes.
        "phoxal-cli-observation",
        "tuirealm",
        "gilrs",
        "webots-proto",
        // Registry and network access.
        "reqwest",
        "cargo",
        "self-replace",
    ] {
        assert!(
            !manifest.contains(forbidden),
            "the supervisor package must not depend on `{forbidden}`"
        );
    }
    // And the ones it must keep: identity, requirement derivation, the catalog,
    // paths, the contract, and the bus.
    for required in [
        "phoxal-cli-core",
        "phoxal-cli-catalog",
        "phoxal-supervisor-api",
        "phoxal-bus",
        "phoxal-manifest",
        "phoxal-runtime-contract",
    ] {
        assert!(
            manifest.contains(required),
            "the supervisor package needs `{required}`"
        );
    }
}
