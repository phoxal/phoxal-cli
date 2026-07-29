pub(crate) mod freshness;
mod ports;
mod runtime;
mod source_group;
pub(crate) mod task_group;
mod transport_set;

use std::sync::Arc;

use anyhow::Result;
use phoxal_cli_core::runtime::RuntimeTarget;
use phoxal_cli_observation::{AttachmentEpoch, AttachmentEvent, SupervisorObservation};
use phoxal_cli_protocol::SupervisorSnapshotV0;
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::state::Stores;
use crate::supervisor::{SupervisorCommands, SupervisorFeed};

pub use ports::AttachmentPorts;
pub use runtime::AttachmentRuntime;
use source_group::SourceGroup;
use transport_set::{GraphTransportSet, robot_keys};

pub struct Attachment {
    pub runtime: AttachmentRuntime,
    pub ports: AttachmentPorts,
}

#[derive(Clone)]
struct AttachmentContext {
    stores: Stores,
    events: mpsc::Sender<AttachmentEvent>,
    freshness: freshness::DeadlineSender,
    cancellation: CancellationToken,
}

pub async fn attach(target: RuntimeTarget) -> Result<Attachment> {
    let feed = SupervisorFeed::connect(target.supervisor_socket.clone()).await?;
    let commands = SupervisorCommands::connect(target.supervisor_socket.clone()).await?;
    let initial = feed.current();
    attach_with_supervisor(target, feed, commands, initial).await
}

pub async fn attach_with_supervisor(
    target: RuntimeTarget,
    feed: SupervisorFeed,
    commands: SupervisorCommands,
    initial: SupervisorSnapshotV0,
) -> Result<Attachment> {
    if let Some(requested) = &target.requested_entry {
        let running = std::path::Path::new(&initial.entry)
            .canonicalize()
            .unwrap_or_else(|_| initial.entry.clone().into());
        anyhow::ensure!(
            &running == requested,
            "entry mismatch: requested {}, but the running entry is {}",
            requested.display(),
            running.display()
        );
    }
    let epoch = epoch_from(&initial);
    let stores = Stores::new(epoch);
    let (event_tx, event_rx) = mpsc::channel(256);
    let (input_tx, input_rx) = mpsc::channel(64);
    let ports = AttachmentPorts::new(event_rx, commands, input_tx, &stores);
    event_tx.send(AttachmentEvent::EpochChanged(epoch)).await?;
    publish_snapshot(&event_tx, &stores, &initial).await?;

    let cancellation = CancellationToken::new();
    let mut tasks = JoinSet::new();
    let runtime_cancellation = cancellation.clone();
    let (freshness_tx, freshness_rx) = mpsc::unbounded_channel();
    let context = AttachmentContext {
        stores,
        events: event_tx.clone(),
        freshness: freshness_tx,
        cancellation: runtime_cancellation,
    };
    tasks.spawn(run_attachment(feed, initial, epoch, input_rx, context));
    tasks.spawn(freshness::run(freshness_rx, event_tx, cancellation.clone()));
    Ok(Attachment {
        runtime: AttachmentRuntime::new(cancellation, tasks),
        ports,
    })
}

async fn run_attachment(
    feed: SupervisorFeed,
    initial: SupervisorSnapshotV0,
    mut epoch: AttachmentEpoch,
    input_rx: mpsc::Receiver<crate::ports::input::InputCommand>,
    context: AttachmentContext,
) {
    let mut snapshots = feed.subscribe();
    let mut roots = robot_keys(&initial);
    freshness::refresh(
        &context.freshness,
        "supervisor",
        phoxal_cli_core::session::DEFAULT_FRESHNESS_TTL,
    );
    let mut source_group = replace_sources(&initial, epoch, input_rx, &context).await;
    if source_group.is_none() {
        context.cancellation.cancel();
        return;
    }
    loop {
        tokio::select! {
            _ = context.cancellation.cancelled() => break,
            changed = snapshots.changed() => {
                if changed.is_err() {
                    break;
                }
                let snapshot = snapshots.borrow_and_update().clone();
                freshness::refresh(
                    &context.freshness,
                    "supervisor",
                    phoxal_cli_core::session::DEFAULT_FRESHNESS_TTL,
                );
                let next_epoch = epoch_from(&snapshot);
                let next_roots = robot_keys(&snapshot);
                if attachment_graph_changed(epoch, &roots, next_epoch, &next_roots) {
                    let input_rx = source_group
                        .take()
                        .expect("attachment source group is installed")
                        .shutdown()
                        .await;
                    context.stores.replace_epoch(next_epoch).await;
                    epoch = next_epoch;
                    roots = next_roots;
                    if context.events.send(AttachmentEvent::EpochChanged(epoch)).await.is_err() {
                        break;
                    }
                    if publish_snapshot(&context.events, &context.stores, &snapshot).await.is_err() {
                        break;
                    }
                    source_group = replace_sources(
                        &snapshot,
                        epoch,
                        input_rx,
                        &context,
                    )
                    .await;
                    if source_group.is_none() {
                        break;
                    }
                    continue;
                }
                if publish_snapshot(&context.events, &context.stores, &snapshot).await.is_err() {
                    break;
                }
            }
        }
    }
    if let Some(source_group) = source_group {
        source_group.shutdown().await;
    }
    feed.shutdown().await;
    context.cancellation.cancel();
}

async fn replace_sources(
    snapshot: &SupervisorSnapshotV0,
    epoch: AttachmentEpoch,
    input_rx: mpsc::Receiver<crate::ports::input::InputCommand>,
    context: &AttachmentContext,
) -> Option<SourceGroup> {
    let transports = loop {
        match GraphTransportSet::open(snapshot, &context.cancellation).await {
            Ok(transports) => break transports,
            Err(error) => {
                if context.cancellation.is_cancelled() {
                    return None;
                }
                tracing::debug!(
                    error = %error,
                    "attachment graph transport is waiting for the epoch router"
                );
                tokio::select! {
                    _ = context.cancellation.cancelled() => return None,
                    _ = tokio::time::sleep(std::time::Duration::from_millis(250)) => {}
                }
            }
        }
    };
    Some(SourceGroup::start(
        epoch,
        snapshot,
        transports,
        context.stores.clone(),
        context.events.clone(),
        input_rx,
        context.freshness.clone(),
    ))
}

async fn publish_snapshot(
    events: &mpsc::Sender<AttachmentEvent>,
    stores: &Stores,
    snapshot: &SupervisorSnapshotV0,
) -> Result<()> {
    let processes = stores.processes.write().await.replace(snapshot);
    events
        .send(AttachmentEvent::SupervisorChanged(Arc::new(
            supervisor_observation(snapshot),
        )))
        .await?;
    events
        .send(AttachmentEvent::ProcessesChanged(Arc::new(processes)))
        .await?;
    Ok(())
}

fn epoch_from(snapshot: &SupervisorSnapshotV0) -> AttachmentEpoch {
    AttachmentEpoch {
        supervisor_generation: snapshot.supervisor_generation,
        execution_id: snapshot.execution_id,
        graph_generation: snapshot.graph_generation,
    }
}

fn attachment_graph_changed(
    current_epoch: AttachmentEpoch,
    current_roots: &std::collections::BTreeSet<phoxal_cli_core::session::RobotKey>,
    next_epoch: AttachmentEpoch,
    next_roots: &std::collections::BTreeSet<phoxal_cli_core::session::RobotKey>,
) -> bool {
    next_epoch != current_epoch || next_roots != current_roots
}

fn supervisor_observation(snapshot: &SupervisorSnapshotV0) -> SupervisorObservation {
    SupervisorObservation {
        supervisor_generation: snapshot.supervisor_generation,
        revision: snapshot.revision,
        execution_id: snapshot.execution_id,
        project: snapshot.project.clone(),
        entry: snapshot.entry.clone(),
        framework_train: snapshot.framework_train.clone(),
        simulation: snapshot.simulation.clone(),
        lifecycle: snapshot.lifecycle,
        router: snapshot.router.clone(),
        plan_revision: snapshot.plan_revision,
        graph_generation: snapshot.graph_generation,
        startup: snapshot.startup.clone(),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::Duration;

    use phoxal_cli_core::runtime::{ResidentAuthority, RuntimeTarget};
    use phoxal_cli_core::session::RobotKey;
    use phoxal_cli_protocol::codec::async_io::{read_frame, write_frame};
    use phoxal_cli_protocol::limits::{
        FRAME_READ_TIMEOUT, FRAME_WRITE_TIMEOUT, MAX_HANDSHAKE_FRAME_BYTES,
        MAX_SNAPSHOT_FRAME_BYTES,
    };
    use phoxal_cli_protocol::{
        CommandSessionId, ConnectionRole, HandshakeReply, HandshakeRequest,
        SUPERVISOR_PROTOCOL_VERSION, SupervisorSnapshot,
    };
    use tokio_util::sync::CancellationToken;

    use super::*;

    #[test]
    fn process_graph_roots_trigger_replacement_without_an_epoch_bump() {
        let epoch = AttachmentEpoch {
            supervisor_generation: 1,
            execution_id: phoxal_cli_core::identity::ExecutionId::mint(),
            graph_generation: 0,
        };
        let empty = std::collections::BTreeSet::new();
        let populated = std::collections::BTreeSet::from([RobotKey::new("lab", "rover")]);
        assert!(attachment_graph_changed(epoch, &empty, epoch, &populated));
        assert!(!attachment_graph_changed(
            epoch, &populated, epoch, &populated
        ));
    }

    #[tokio::test]
    async fn two_simultaneous_attachments_have_independent_runtime_and_ports() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("supervisor.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let stop = CancellationToken::new();
        let server_stop = stop.clone();
        let server = tokio::spawn(async move {
            for index in 0..4_u8 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let connection_stop = server_stop.clone();
                tokio::spawn(async move {
                    let handshake: HandshakeRequest =
                        read_frame(&mut stream, MAX_HANDSHAKE_FRAME_BYTES, FRAME_READ_TIMEOUT)
                            .await
                            .unwrap();
                    write_frame(
                        &mut stream,
                        &HandshakeReply {
                            protocol_version: SUPERVISOR_PROTOCOL_VERSION,
                            supervisor_generation: 7,
                            command_session: (handshake.role == ConnectionRole::Commands)
                                .then_some(CommandSessionId([index; 16])),
                        },
                        MAX_HANDSHAKE_FRAME_BYTES,
                        FRAME_WRITE_TIMEOUT,
                    )
                    .await
                    .unwrap();
                    if handshake.role == ConnectionRole::Snapshots {
                        write_frame(
                            &mut stream,
                            &SupervisorSnapshot::V0(SupervisorSnapshotV0 {
                                supervisor_generation: 7,
                                ..SupervisorSnapshotV0::default()
                            }),
                            MAX_SNAPSHOT_FRAME_BYTES,
                            FRAME_WRITE_TIMEOUT,
                        )
                        .await
                        .unwrap();
                    }
                    connection_stop.cancelled().await;
                });
            }
        });
        let target = RuntimeTarget {
            logical_root: directory.path().to_path_buf(),
            requested_entry: None,
            project_lock: PathBuf::new(),
            supervisor_socket: socket,
            zenoh_socket: PathBuf::new(),
            zenoh_endpoint: String::new(),
            authority: ResidentAuthority::DetachedSession,
        };
        let (left, right) = tokio::join!(attach(target.clone()), attach(target));
        let left = left.unwrap();
        let right = right.unwrap();
        assert!(!std::ptr::eq(&left.ports.events, &right.ports.events));
        tokio::time::timeout(Duration::from_secs(1), left.runtime.shutdown())
            .await
            .expect("first attachment shutdown");
        tokio::time::timeout(Duration::from_secs(1), right.runtime.shutdown())
            .await
            .expect("second attachment shutdown");
        stop.cancel();
        server.await.unwrap();
    }
}
