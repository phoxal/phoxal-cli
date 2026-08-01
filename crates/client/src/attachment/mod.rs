pub(crate) mod freshness;
mod ports;
mod runtime;
mod source_group;
pub(crate) mod task_group;
mod transport_set;

use std::sync::Arc;

use anyhow::Result;
use phoxal_cli_core::runtime::{ProjectLifecycle, RuntimeTarget};
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
pub(crate) use source_group::SourceRetry;
use transport_set::{GraphTransportSet, robot_keys};

pub(crate) const DEFAULT_FRESHNESS_TTL: std::time::Duration = std::time::Duration::from_secs(3);

pub struct Attachment {
    pub runtime: AttachmentRuntime,
    pub ports: AttachmentPorts,
}

#[derive(Clone)]
struct AttachmentContext {
    stores: Stores,
    events: mpsc::Sender<AttachmentEvent>,
    freshness: freshness::Scheduler,
    cancellation: CancellationToken,
}

pub async fn attach_with_supervisor(
    target: RuntimeTarget,
    feed: SupervisorFeed,
    commands: SupervisorCommands,
    initial: SupervisorSnapshotV0,
) -> Result<Attachment> {
    validate_requested_entry(&target, &initial.entry)?;
    let epoch = epoch_from(&initial);
    let stores = Stores::new(epoch);
    let (event_tx, event_rx) = mpsc::channel(256);
    let (input_tx, input_rx) = mpsc::channel(64);
    let ports = AttachmentPorts::new(event_rx, commands, input_tx, &stores);
    event_tx.send(AttachmentEvent::EpochChanged(epoch)).await?;
    event_tx
        .send(AttachmentEvent::ConnectionChanged(
            feed.connection().borrow().clone(),
        ))
        .await?;
    publish_snapshot(&event_tx, &stores, &initial).await?;

    let cancellation = CancellationToken::new();
    let mut tasks = JoinSet::new();
    let runtime_cancellation = cancellation.clone();
    let freshness = freshness::Scheduler::new(epoch);
    let context = AttachmentContext {
        stores,
        events: event_tx.clone(),
        freshness: freshness.clone(),
        cancellation: runtime_cancellation,
    };
    tasks.spawn(run_attachment(feed, initial, epoch, input_rx, context));
    tasks.spawn(freshness::run(freshness, event_tx, cancellation.clone()));
    Ok(Attachment {
        runtime: AttachmentRuntime::new(cancellation, tasks),
        ports,
    })
}

pub fn validate_requested_entry(target: &RuntimeTarget, running_entry: &str) -> Result<()> {
    let Some(requested) = &target.requested_entry else {
        return Ok(());
    };
    let running = std::path::Path::new(running_entry)
        .canonicalize()
        .unwrap_or_else(|_| running_entry.into());
    anyhow::ensure!(
        &running == requested,
        "entry mismatch: requested {}, but the running entry is {}",
        requested.display(),
        running.display()
    );
    Ok(())
}

#[cfg(test)]
async fn attach(target: RuntimeTarget) -> Result<Attachment> {
    let feed = SupervisorFeed::connect(target.supervisor_socket.clone()).await?;
    let commands = SupervisorCommands::connect(target.supervisor_socket.clone()).await?;
    let initial = feed.current();
    attach_with_supervisor(target, feed, commands, initial).await
}

async fn run_attachment(
    feed: SupervisorFeed,
    initial: SupervisorSnapshotV0,
    mut epoch: AttachmentEpoch,
    input_rx: mpsc::Receiver<crate::ports::input::InputCommand>,
    context: AttachmentContext,
) {
    let mut snapshots = feed.subscribe();
    let mut connection = feed.connection();
    let mut current_snapshot = initial.clone();
    let mut roots = robot_keys(&initial);
    context.freshness.refresh(
        epoch,
        "supervisor",
        crate::attachment::DEFAULT_FRESHNESS_TTL,
    );
    let mut input_rx = Some(input_rx);
    let mut source_group: Option<SourceGroup> = None;
    let mut connection_open = true;
    let mut reopen_backoff = reopen_backoff();
    let mut reopen_delay: Option<std::pin::Pin<Box<tokio::time::Sleep>>> = None;
    let mut opening = Some(OpenAttempt::spawn(
        initial.clone(),
        epoch,
        context.cancellation.clone(),
    ));
    loop {
        tokio::select! {
            biased;
            _ = context.cancellation.cancelled() => break,
            opened = async {
                (&mut opening
                    .as_mut()
                    .expect("opening branch is guarded")
                    .task)
                    .await
            }, if opening.is_some() => {
                let attempt = opening.take().expect("completed opening exists");
                let mut retry_open = false;
                match opened {
                    Ok(Some(transports)) if attempt.epoch == epoch => {
                        source_group = Some(SourceGroup::start(
                            epoch,
                            &current_snapshot,
                            transports,
                            context.stores.clone(),
                            context.events.clone(),
                            input_rx.take().expect("input receiver is parked while opening"),
                            context.freshness.clone(),
                        ));
                    }
                    Ok(Some(transports)) => transports.close().await,
                    Ok(None) => retry_open = !context.cancellation.is_cancelled(),
                    Err(error) if error.is_cancelled() => {}
                    Err(error) => {
                        tracing::debug!(error = %error, "attachment transport opener stopped");
                        retry_open = !context.cancellation.is_cancelled();
                    }
                }
                if retry_open {
                    reopen_delay = Some(schedule_reopen(&mut reopen_backoff));
                }
            }
            () = async {
                source_group
                    .as_mut()
                    .expect("source-group branch is guarded")
                    .reopen_requested()
                    .await
            }, if source_group.is_some() && opening.is_none() => {
                let group = source_group.take().expect("source group exists");
                input_rx = Some(group.shutdown().await);
                reopen_delay = Some(schedule_reopen(&mut reopen_backoff));
            }
            () = async {
                reopen_delay
                    .as_mut()
                    .expect("reopen-delay branch is guarded")
                    .await
            }, if reopen_delay.is_some() && opening.is_none() && source_group.is_none() => {
                reopen_delay = None;
                opening = Some(OpenAttempt::spawn(
                    current_snapshot.clone(),
                    epoch,
                    context.cancellation.clone(),
                ));
            }
            changed = connection.changed(), if connection_open => {
                let observation = connection.borrow_and_update().clone();
                let lost = matches!(
                    &observation,
                    phoxal_cli_observation::ConnectionObservation::Lost { .. }
                );
                if context
                    .events
                    .send(AttachmentEvent::ConnectionChanged(observation))
                    .await
                    .is_err()
                {
                    break;
                }
                if lost {
                    break;
                }
                if changed.is_err() {
                    connection_open = false;
                }
            }
            changed = snapshots.changed() => {
                if changed.is_err() {
                    break;
                }
                let snapshot = snapshots.borrow_and_update().clone();
                current_snapshot = snapshot.clone();
                context.freshness.refresh(
                    epoch,
                    "supervisor",
                    crate::attachment::DEFAULT_FRESHNESS_TTL,
                );
                let next_epoch = epoch_from(&snapshot);
                let next_roots = robot_keys(&snapshot);
                let terminal = matches!(
                    snapshot.lifecycle,
                    ProjectLifecycle::Stopped | ProjectLifecycle::Failed
                );
                if attachment_graph_changed(epoch, &roots, next_epoch, &next_roots) {
                    context.stores.replace_epoch(next_epoch).await;
                    epoch = next_epoch;
                    roots = next_roots;
                    context.freshness.reset(epoch);
                    context.freshness.refresh(
                        epoch,
                        "supervisor",
                        crate::attachment::DEFAULT_FRESHNESS_TTL,
                    );
                    if context.events.send(AttachmentEvent::EpochChanged(epoch)).await.is_err() {
                        break;
                    }
                    if publish_snapshot(&context.events, &context.stores, &snapshot).await.is_err() {
                        break;
                    }
                    if let Some(attempt) = opening.take() {
                        attempt.cancel().await;
                    }
                    reopen_delay = None;
                    reopen_backoff.reset();
                    if let Some(group) = source_group.take() {
                        input_rx = Some(group.shutdown().await);
                    }
                    if terminal {
                        break;
                    }
                    opening = Some(OpenAttempt::spawn(
                        current_snapshot.clone(),
                        epoch,
                        context.cancellation.clone(),
                    ));
                    continue;
                }
                if publish_snapshot(&context.events, &context.stores, &snapshot).await.is_err() {
                    break;
                }
                if terminal {
                    break;
                }
            }
        }
    }
    if let Some(attempt) = opening {
        attempt.cancel().await;
    }
    if let Some(source_group) = source_group {
        let _ = source_group.shutdown().await;
    }
    feed.shutdown().await;
    context.cancellation.cancel();
}

fn reopen_backoff() -> crate::reconcile::RetryBackoff {
    crate::reconcile::RetryBackoff::new(
        std::time::Duration::from_millis(250),
        std::time::Duration::from_secs(2),
    )
}

fn schedule_reopen(
    backoff: &mut crate::reconcile::RetryBackoff,
) -> std::pin::Pin<Box<tokio::time::Sleep>> {
    Box::pin(tokio::time::sleep(backoff.next_delay()))
}

struct OpenAttempt {
    epoch: AttachmentEpoch,
    cancellation: CancellationToken,
    task: tokio::task::JoinHandle<Option<GraphTransportSet>>,
}

impl OpenAttempt {
    fn spawn(
        snapshot: SupervisorSnapshotV0,
        epoch: AttachmentEpoch,
        parent_cancellation: CancellationToken,
    ) -> Self {
        let cancellation = parent_cancellation.child_token();
        let open_cancellation = cancellation.clone();
        let task = tokio::spawn(async move {
            loop {
                match GraphTransportSet::open(&snapshot, &open_cancellation).await {
                    Ok(transports) => return Some(transports),
                    Err(error) => {
                        if open_cancellation.is_cancelled() {
                            return None;
                        }
                        tracing::debug!(
                            error = %error,
                            "attachment graph transport is waiting for the epoch router"
                        );
                        tokio::select! {
                            _ = open_cancellation.cancelled() => return None,
                            _ = tokio::time::sleep(std::time::Duration::from_millis(250)) => {}
                        }
                    }
                }
            }
        });
        Self {
            epoch,
            cancellation,
            task,
        }
    }

    async fn cancel(self) {
        self.cancellation.cancel();
        if let Ok(Some(transports)) = self.task.await {
            transports.close().await;
        }
    }
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
        .send(AttachmentEvent::ProcessesChanged {
            epoch: epoch_from(snapshot),
            values: Arc::new(processes),
        })
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
    current_roots: &std::collections::BTreeSet<phoxal_cli_core::runtime::RobotKey>,
    next_epoch: AttachmentEpoch,
    next_roots: &std::collections::BTreeSet<phoxal_cli_core::runtime::RobotKey>,
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
        graph_generation: snapshot.graph_generation,
        startup: snapshot.startup.clone(),
        failure: snapshot.failure.clone(),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use phoxal_cli_core::runtime::{
        ParticipantKind, ProcessDescriptor, ProcessEntry, ProcessKey, ProcessStatus,
        ProjectLifecycle, ResidentAuthority, RobotKey, RuntimeTarget, StartupRequirement,
    };
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

    #[test]
    fn graph_reopen_pacing_caps_and_resets_at_the_epoch_boundary() {
        let mut backoff = reopen_backoff();
        assert_eq!(backoff.next_delay(), Duration::from_millis(250));
        assert_eq!(backoff.next_delay(), Duration::from_millis(500));
        assert_eq!(backoff.next_delay(), Duration::from_secs(1));
        assert_eq!(backoff.next_delay(), Duration::from_secs(2));
        assert_eq!(backoff.next_delay(), Duration::from_secs(2));
        backoff.reset();
        assert_eq!(backoff.next_delay(), Duration::from_millis(250));
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

    #[tokio::test]
    async fn terminal_snapshot_is_published_while_graph_opening_retries() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("supervisor.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let stop = CancellationToken::new();
        let server_stop = stop.clone();
        let execution_id = phoxal_cli_core::identity::ExecutionId::mint();
        let mut initial = SupervisorSnapshotV0 {
            supervisor_generation: 7,
            execution_id,
            router: "not-a-valid-zenoh-endpoint".into(),
            lifecycle: ProjectLifecycle::Ready,
            ..SupervisorSnapshotV0::default()
        };
        let key = ProcessKey::robot(RobotKey::new("lab", "rover"), "drive");
        initial.processes.insert(
            key.clone(),
            ProcessEntry {
                descriptor: ProcessDescriptor {
                    key,
                    kind: ParticipantKind::Service,
                    artifact: "drive".into(),
                    owner: "project".into(),
                    startup_requirement: StartupRequirement::Required,
                },
                status: ProcessStatus::default(),
            },
        );
        let terminal = SupervisorSnapshotV0 {
            revision: initial.revision + 1,
            lifecycle: ProjectLifecycle::Stopped,
            ..initial.clone()
        };
        let server = tokio::spawn(async move {
            for index in 0..2_u8 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let connection_stop = server_stop.clone();
                let initial = initial.clone();
                let terminal = terminal.clone();
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
                            &SupervisorSnapshot::V0(initial),
                            MAX_SNAPSHOT_FRAME_BYTES,
                            FRAME_WRITE_TIMEOUT,
                        )
                        .await
                        .unwrap();
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        write_frame(
                            &mut stream,
                            &SupervisorSnapshot::V0(terminal),
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
        let mut attachment = attach(target).await.unwrap();
        let terminal = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let event = attachment.ports.events.recv().await.expect("event stream");
                if matches!(
                    event,
                    AttachmentEvent::SupervisorChanged(ref supervisor)
                        if supervisor.lifecycle == ProjectLifecycle::Stopped
                ) {
                    break event;
                }
            }
        })
        .await
        .expect("terminal observation must not wait for graph opening");
        assert!(matches!(
            terminal,
            AttachmentEvent::SupervisorChanged(supervisor)
                if supervisor.lifecycle == ProjectLifecycle::Stopped
        ));
        attachment.runtime.shutdown().await;
        stop.cancel();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn restarted_resident_is_always_reported_lost_before_feed_closure() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("supervisor.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let snapshot_connections = Arc::new(AtomicUsize::new(0));
        let server_snapshots = snapshot_connections.clone();
        let stop = CancellationToken::new();
        let server_stop = stop.clone();
        let server = tokio::spawn(async move {
            for index in 0..3_u8 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let connection_stop = server_stop.clone();
                let snapshot_index = server_snapshots.clone();
                tokio::spawn(async move {
                    let handshake: HandshakeRequest =
                        read_frame(&mut stream, MAX_HANDSHAKE_FRAME_BYTES, FRAME_READ_TIMEOUT)
                            .await
                            .unwrap();
                    let reconnect = handshake.role == ConnectionRole::Snapshots
                        && snapshot_index.fetch_add(1, Ordering::SeqCst) > 0;
                    let generation = if reconnect { 8 } else { 7 };
                    write_frame(
                        &mut stream,
                        &HandshakeReply {
                            protocol_version: SUPERVISOR_PROTOCOL_VERSION,
                            supervisor_generation: generation,
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
                                supervisor_generation: generation,
                                entry: "/tmp/robot/robot.yaml".into(),
                                lifecycle: ProjectLifecycle::Ready,
                                ..SupervisorSnapshotV0::default()
                            }),
                            MAX_SNAPSHOT_FRAME_BYTES,
                            FRAME_WRITE_TIMEOUT,
                        )
                        .await
                        .unwrap();
                        if !reconnect {
                            return;
                        }
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
        let mut attachment = attach(target).await.unwrap();
        let lost = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let event = attachment.ports.events.recv().await.expect("event stream");
                if matches!(
                    event,
                    AttachmentEvent::ConnectionChanged(
                        phoxal_cli_observation::ConnectionObservation::Lost { .. }
                    )
                ) {
                    break event;
                }
            }
        })
        .await
        .expect("lost identity must be delivered before watch closure");
        assert!(matches!(
            lost,
            AttachmentEvent::ConnectionChanged(
                phoxal_cli_observation::ConnectionObservation::Lost { .. }
            )
        ));
        attachment.runtime.shutdown().await;
        stop.cancel();
        server.await.unwrap();
    }
}
