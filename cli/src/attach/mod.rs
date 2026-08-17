//! The disposable attachment session.
//!
//! One [`Session`] is one attachment to one execution: it opens a
//! [`phoxal_client::Connection`] and runs the feeds the terminal application
//! renders. Everything remote arrives through `phoxal_client`; this crate
//! names no wire crate of its own.
//!
//! There is no private IPC anywhere below. Every fact this
//! session has comes off the execution-scoped Zenoh supervisor API or out of
//! hardware attached to this machine; nothing is read from a socket the supervisor
//! and the client agreed on privately, and there is no snapshot, command, or
//! log channel that is not part of the published contract.
//!
//! A new execution is a new attachment. The epoch a store update carries *is*
//! the `ExecutionId`, so a supervisor that restarted cannot have its rows spliced
//! onto the previous run's - the session ends and the caller opens a new one.

pub(crate) mod feeds;
pub(crate) mod ports;
pub(crate) mod state;

use std::sync::{Arc, Mutex, PoisonError};

use anyhow::{Context, Result};
use phoxal_cli_observation::{
    AttachmentEpoch, AttachmentEvent, ConnectionObservation, LocalRuntimes,
};
use phoxal_client::supervisor::{self, logs};
use phoxal_client::{Client, ConnectOptions, Connection, DisconnectReason, StreamReceiver};
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use ports::{
    events::AttachmentEvents, input::InputCommands, logs::LogReader, runtimes::RuntimeReader,
};
use state::Stores;

/// This client's diagnostic label in bus metadata. It is never identity: the
/// producer is the session's own ZID.
pub(crate) const CLIENT_PARTICIPANT: &str = "phoxal-cli-attachment";

const EVENT_CAPACITY: usize = 256;
const INPUT_CAPACITY: usize = 64;

/// What the client that launched a session knows about its own children.
///
/// The supervisor publishes presence and nothing else, deliberately: it started
/// no process, so it cannot say why one is absent. This is the other half, and
/// it is empty for an attachment to an execution this client did not launch.
#[derive(Clone, Default)]
pub(crate) struct LocalRuntimeFacts(Arc<Mutex<LocalRuntimes>>);

impl LocalRuntimeFacts {
    pub(crate) fn read(&self) -> LocalRuntimes {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    pub(crate) fn replace(&self, values: LocalRuntimes) {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = values;
    }
}

/// The ports the terminal application drives a session through.
///
/// There is no supervisor-command port. The supervisor observes: it has no
/// `stop` and no `restart` to send, and the only commands it does take -
/// reboot, power off - are host operations no dashboard key emits.
pub(crate) struct SessionPorts {
    pub(crate) events: AttachmentEvents,
    pub(crate) input_commands: InputCommands,
    pub(crate) logs: LogReader,
    pub(crate) runtimes: RuntimeReader,
}

/// One live attachment plus the feeds that project it.
pub(crate) struct Session {
    pub(crate) ports: SessionPorts,
    connection: Connection,
    client: Client,
    local: LocalRuntimeFacts,
    cancellation: CancellationToken,
    tasks: JoinSet<()>,
}

impl Session {
    pub(crate) fn connected(&self) -> &phoxal_client::Connected {
        self.connection.connected()
    }

    /// What this client knows about the runtimes it launched itself.
    pub(crate) const fn local(&self) -> &LocalRuntimeFacts {
        &self.local
    }

    /// Attach at `endpoint` and start every feed.
    ///
    /// # Errors
    ///
    /// Any direct handshake or supervisor-contract failure.
    pub(crate) async fn open(
        endpoint: &str,
        project: String,
        local: LocalRuntimeFacts,
    ) -> Result<Self> {
        let connection =
            Connection::connect(&ConnectOptions::new(endpoint, CLIENT_PARTICIPANT)).await?;
        let client = connection.client();
        let epoch = AttachmentEpoch::new(connection.execution());
        let stores = Stores::new(epoch);
        let (events_tx, events_rx) = mpsc::channel(EVENT_CAPACITY);
        let (input_tx, input_rx) = mpsc::channel(INPUT_CAPACITY);

        // The opening events: the epoch this attachment observes, and the fact
        // that it is connected. They are queued before any feed starts, so the
        // channel is empty and `try_send` cannot block; a failure here means
        // the channel was built too small to hold them, which is this module's
        // bug and is reported rather than panicked on.
        for event in [
            AttachmentEvent::EpochChanged(epoch),
            AttachmentEvent::ConnectionChanged(ConnectionObservation::Connected),
        ] {
            events_tx.try_send(event).context(
                "the attachment event channel could not accept the opening events; \
                 EVENT_CAPACITY is too small",
            )?;
        }

        let cancellation = CancellationToken::new();
        let mut tasks = JoinSet::new();
        let context = feeds::FeedContext {
            client: client.clone(),
            epoch,
            project,
            local: local.clone(),
            stores: stores.clone(),
            events: events_tx,
            cancellation: cancellation.clone(),
        };
        feeds::spawn_all(&mut tasks, context, input_rx);

        Ok(Self {
            ports: SessionPorts {
                events: AttachmentEvents::new(events_rx),
                input_commands: InputCommands::new(input_tx),
                logs: LogReader::new(stores.logs.clone()),
                runtimes: RuntimeReader::new(stores.runtimes.clone()),
            },
            connection,
            client,
            local,
            cancellation,
            tasks,
        })
    }

    /// Observe the highest-revision snapshot this session has installed.
    pub(crate) fn snapshots(
        &self,
    ) -> tokio::sync::watch::Receiver<Option<supervisor::execution::Snapshot>> {
        self.client.snapshots()
    }

    /// The most recent snapshot, without awaiting.
    pub(crate) fn snapshot(&self) -> Option<supervisor::execution::Snapshot> {
        self.client.snapshot()
    }

    /// Resolve with the first structured cause that ended the connection.
    pub(crate) async fn disconnected(&self) -> DisconnectReason {
        self.client.disconnected().await
    }

    /// One backward page of the supervisor's bounded log history.
    pub(crate) async fn logs(
        &self,
        participant_id: Option<String>,
        limit: u32,
        before_sequence: Option<u64>,
    ) -> Result<logs::Snapshot> {
        Ok(self
            .client
            .logs(participant_id, limit, before_sequence)
            .await?)
    }

    /// The live log stream that continues a [`Self::logs`] page.
    pub(crate) async fn follow_logs(
        &self,
    ) -> Result<StreamReceiver<supervisor::endpoint::logs::FollowEndpoint>> {
        Ok(self.client.follow_logs().await?)
    }

    /// Detach: stop every feed, then close the session. The supervisor is
    /// unaffected - detaching is not stopping.
    pub(crate) async fn shutdown(self) {
        if let Err(error) = self.close().await {
            tracing::debug!(error = %format!("{error:#}"), "closing the attachment session failed");
        }
    }

    /// Close the attachment and preserve deterministic local close failures.
    ///
    /// Lifecycle commands use this form when a clean close is part of the
    /// command's claimed outcome. Disposable UI detach paths use
    /// [`Self::shutdown`] and retain their existing best-effort behavior.
    pub(crate) async fn close(mut self) -> Result<()> {
        self.cancellation.cancel();
        self.tasks.shutdown().await;
        self.connection.close().await?;
        Ok(())
    }
}
