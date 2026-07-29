use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use phoxal_cli_core::runtime::ProjectLifecycle;
use phoxal_cli_protocol::codec::async_io::{read_frame, read_frame_after_idle};
use phoxal_cli_protocol::limits::{FRAME_READ_TIMEOUT, MAX_SNAPSHOT_FRAME_BYTES};
use phoxal_cli_protocol::{ConnectionRole, SupervisorSnapshot, SupervisorSnapshotV0};
use tokio::net::UnixStream;
use tokio::sync::{Mutex, watch};
use tokio_util::sync::CancellationToken;

use super::connection::connect_role;

const RECONNECT_DELAY: Duration = Duration::from_millis(200);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionState {
    Connected,
    Reconnecting,
    Terminal,
    Crashed,
}

#[derive(Clone)]
pub struct SupervisorFeed {
    snapshots: watch::Receiver<SupervisorSnapshotV0>,
    connection: watch::Receiver<ConnectionState>,
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
        let (snapshot_tx, snapshots) = watch::channel(initial);
        let (connection_tx, connection) = watch::channel(ConnectionState::Connected);
        let lifetime = Arc::new(FeedLifetime {
            cancellation: CancellationToken::new(),
            task: Mutex::new(None),
        });
        let task = tokio::spawn(snapshot_loop(
            path,
            stream,
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
    pub fn connection(&self) -> watch::Receiver<ConnectionState> {
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
    snapshots: watch::Sender<SupervisorSnapshotV0>,
    connection: watch::Sender<ConnectionState>,
    cancellation: CancellationToken,
) {
    loop {
        let received = tokio::select! {
            _ = cancellation.cancelled() => return,
            received = read_snapshot_frame(&mut stream) => received,
        };
        match received {
            Ok(snapshot) => {
                snapshots.send_replace(snapshot);
                connection.send_replace(ConnectionState::Connected);
            }
            Err(_) => {
                let terminal = matches!(
                    snapshots.borrow().lifecycle,
                    ProjectLifecycle::Stopped | ProjectLifecycle::Failed
                );
                if terminal {
                    connection.send_replace(ConnectionState::Terminal);
                    return;
                }
                connection.send_replace(ConnectionState::Reconnecting);
                tokio::select! {
                    _ = cancellation.cancelled() => return,
                    _ = tokio::time::sleep(RECONNECT_DELAY) => {}
                }
                match connect_role(&path, ConnectionRole::Snapshots, None).await {
                    Ok((new_stream, handshake)) => {
                        stream = new_stream;
                        match read_snapshot_frame(&mut stream).await {
                            Ok(snapshot)
                                if snapshot.supervisor_generation
                                    == handshake.supervisor_generation =>
                            {
                                snapshots.send_replace(snapshot);
                                connection.send_replace(ConnectionState::Connected);
                            }
                            _ => {
                                connection.send_replace(ConnectionState::Crashed);
                                return;
                            }
                        }
                    }
                    Err(_) => {
                        connection.send_replace(ConnectionState::Crashed);
                        return;
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
