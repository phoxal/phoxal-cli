use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use phoxal_cli_core::runtime::ProjectLifecycle;
use phoxal_cli_observation::ConnectionObservation;
use phoxal_cli_protocol::codec::async_io::{read_frame, read_frame_after_idle};
use phoxal_cli_protocol::limits::{FRAME_READ_TIMEOUT, MAX_SNAPSHOT_FRAME_BYTES};
use phoxal_cli_protocol::{ConnectionRole, SupervisorSnapshot, SupervisorSnapshotV0};
use tokio::net::UnixStream;
use tokio::sync::{Mutex, watch};
use tokio_util::sync::CancellationToken;

use super::connection::{connect_role, is_connection_unavailable};
use crate::reconcile::RetryBackoff;

const INITIAL_RECONNECT_DELAY: Duration = Duration::from_millis(100);
const MAX_RECONNECT_DELAY: Duration = Duration::from_secs(2);

struct ExpectedResident {
    generation: u64,
    entry: String,
    project: String,
}

#[derive(Clone)]
pub struct SupervisorFeed {
    snapshots: watch::Receiver<SupervisorSnapshotV0>,
    connection: watch::Receiver<ConnectionObservation>,
    _lifetime: Arc<FeedLifetime>,
}

struct FeedLifetime {
    cancellation: CancellationToken,
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl Drop for FeedLifetime {
    fn drop(&mut self) {
        self.cancellation.cancel();
    }
}

impl SupervisorFeed {
    pub async fn connect(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        let (mut stream, handshake) = connect_role(&path, ConnectionRole::Snapshots, None).await?;
        let initial = read_frame::<_, SupervisorSnapshot>(
            &mut stream,
            MAX_SNAPSHOT_FRAME_BYTES,
            FRAME_READ_TIMEOUT,
        )
        .await
        .context("read initial supervisor snapshot")?
        .into_v0();
        anyhow::ensure!(
            initial.supervisor_generation == handshake.supervisor_generation,
            "snapshot generation does not match handshake"
        );
        let expected = ExpectedResident {
            generation: initial.supervisor_generation,
            entry: initial.entry.clone(),
            project: initial.project.clone(),
        };
        let (snapshot_tx, snapshots) = watch::channel(initial);
        let (connection_tx, connection) = watch::channel(ConnectionObservation::Connected);
        let lifetime = Arc::new(FeedLifetime {
            cancellation: CancellationToken::new(),
            task: Mutex::new(None),
        });
        let task = tokio::spawn(snapshot_loop(
            path,
            stream,
            expected,
            snapshot_tx,
            connection_tx,
            lifetime.cancellation.clone(),
        ));
        *lifetime.task.lock().await = Some(task);
        Ok(Self {
            snapshots,
            connection,
            _lifetime: lifetime,
        })
    }

    #[must_use]
    pub fn current(&self) -> SupervisorSnapshotV0 {
        self.snapshots.borrow().clone()
    }

    #[must_use]
    pub fn subscribe(&self) -> watch::Receiver<SupervisorSnapshotV0> {
        self.snapshots.clone()
    }

    #[must_use]
    pub fn connection(&self) -> watch::Receiver<ConnectionObservation> {
        self.connection.clone()
    }

    pub(crate) async fn shutdown(&self) {
        self._lifetime.cancellation.cancel();
        let task = self._lifetime.task.lock().await.take();
        if let Some(task) = task {
            let _ = task.await;
        }
    }
}

async fn snapshot_loop(
    path: PathBuf,
    mut stream: UnixStream,
    expected: ExpectedResident,
    snapshots: watch::Sender<SupervisorSnapshotV0>,
    connection: watch::Sender<ConnectionObservation>,
    cancellation: CancellationToken,
) {
    let mut backoff = RetryBackoff::new(INITIAL_RECONNECT_DELAY, MAX_RECONNECT_DELAY);
    let mut attempt = 0_u32;
    loop {
        let received = tokio::select! {
            _ = cancellation.cancelled() => return,
            received = read_snapshot_frame(&mut stream) => received,
        };
        match received {
            Ok(snapshot) => {
                snapshots.send_replace(snapshot);
                connection.send_replace(ConnectionObservation::Connected);
                attempt = 0;
                backoff.reset();
            }
            Err(error) => {
                tracing::debug!(error = %error, "supervisor snapshot feed disconnected");
                let terminal = matches!(
                    snapshots.borrow().lifecycle,
                    ProjectLifecycle::Stopped | ProjectLifecycle::Failed
                );
                if terminal {
                    connection.send_replace(ConnectionObservation::Terminal);
                    return;
                }
                loop {
                    attempt = attempt.saturating_add(1);
                    connection.send_replace(ConnectionObservation::Reconnecting { attempt });
                    tokio::select! {
                        _ = cancellation.cancelled() => return,
                        _ = tokio::time::sleep(backoff.next_delay()) => {}
                    }
                    let connected = connect_role(&path, ConnectionRole::Snapshots, None).await;
                    let (new_stream, handshake) = match connected {
                        Ok(connected) => connected,
                        Err(error) if is_connection_unavailable(&error) => {
                            // An attached session remains authoritative until
                            // cancellation or a permanent identity/protocol
                            // failure. The UI exposes this attempt count in its
                            // global header, so a router/resident outage is
                            // recoverable without becoming silent.
                            tracing::debug!(
                                attempt,
                                error = %error,
                                "supervisor snapshot feed is waiting for the resident"
                            );
                            continue;
                        }
                        Err(error) => {
                            connection.send_replace(ConnectionObservation::Lost {
                                reason: format!("{error:#}").into(),
                            });
                            return;
                        }
                    };
                    stream = new_stream;
                    match read_snapshot_frame(&mut stream).await {
                        Ok(snapshot)
                            if snapshot.supervisor_generation
                                == handshake.supervisor_generation
                                && snapshot.supervisor_generation == expected.generation
                                && snapshot.entry == expected.entry
                                && snapshot.project == expected.project =>
                        {
                            snapshots.send_replace(snapshot);
                            connection.send_replace(ConnectionObservation::Connected);
                            attempt = 0;
                            backoff.reset();
                            break;
                        }
                        Ok(_) => {
                            connection.send_replace(ConnectionObservation::Lost {
                                reason: "reconnected resident identity does not match the attached session"
                                    .into(),
                            });
                            return;
                        }
                        Err(error) => {
                            tracing::debug!(
                                attempt,
                                error = %error,
                                "reconnected supervisor closed before a snapshot"
                            );
                        }
                    }
                }
            }
        }
    }
}

async fn read_snapshot_frame(stream: &mut UnixStream) -> Result<SupervisorSnapshotV0> {
    read_frame_after_idle::<_, SupervisorSnapshot>(
        stream,
        MAX_SNAPSHOT_FRAME_BYTES,
        FRAME_READ_TIMEOUT,
    )
    .await
    .map(SupervisorSnapshot::into_v0)
}
