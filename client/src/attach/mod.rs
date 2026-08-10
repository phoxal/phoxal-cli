//! The disposable attachment session.
//!
//! One [`Session`] is one attachment to one execution: it opens a
//! [`phoxal_supervisor_client::Attachment`] and runs the feeds the terminal
//! application renders.
//!
//! There is no private IPC anywhere below. Every fact this
//! session has comes off the execution-scoped Zenoh supervisor API or out of
//! hardware attached to this machine; nothing is read from a socket the daemon
//! and the client agreed on privately, and there is no snapshot, command, or
//! log channel that is not part of the published contract.
//!
//! A new execution is a new attachment. The epoch a store update carries *is*
//! the `ExecutionId`, so a daemon that restarted cannot have its rows spliced
//! onto the previous run's - the session ends and the caller opens a new one.

pub(crate) mod feeds;
pub(crate) mod ports;
pub(crate) mod state;

use anyhow::{Context, Result};
use phoxal_api::supervisor::{self, command::CommandOutcome, logs};
use phoxal_cli_observation::{AttachmentEpoch, AttachmentEvent, ConnectionObservation};
use phoxal_runtime_contract::identity::{ParticipantId, ProducerId};
use phoxal_supervisor_client::{Attachment, AttachmentConfig, AttachmentPort};
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::joypad::manual::ManualDrive;
use ports::{AttachmentEvents, InputCommands, LogReader, RuntimeReader};
use state::Stores;

/// This client's diagnostic label in bus metadata. It is never identity: the
/// producer is the session's own ZID.
pub(crate) const CLIENT_PARTICIPANT: &str = "phoxal-cli-attachment";

const EVENT_CAPACITY: usize = 256;
const INPUT_CAPACITY: usize = 64;

/// A command channel to the attached supervisor.
///
/// It is a handle over the shared [`Attachment`], not a second connection: the
/// supervisor API is one query topic, so there is nothing to open twice.
#[derive(Clone)]
pub(crate) struct SupervisorCommands {
    attachment: AttachmentPort,
}

impl SupervisorCommands {
    /// Restart one process, fenced on the producer the caller last observed.
    pub(crate) async fn restart(
        &self,
        process: ParticipantId,
        expected_producer: ProducerId,
    ) -> Result<CommandOutcome> {
        Ok(self.attachment.restart(process, expected_producer).await?)
    }

    /// End the execution.
    ///
    /// The one place this client ends an execution: `phoxal stop`, the TUI's
    /// stop effect, and the simulation session's "the world clock is gone"
    /// path all go through here, so there is exactly one stop contract to
    /// reason about.
    pub(crate) async fn stop(&self) -> Result<CommandOutcome> {
        Ok(self.attachment.stop().await?)
    }
}

/// The ports the terminal application drives a session through.
pub(crate) struct SessionPorts {
    pub(crate) events: AttachmentEvents,
    pub(crate) supervisor: SupervisorCommands,
    pub(crate) input_commands: InputCommands,
    pub(crate) logs: LogReader,
    pub(crate) runtimes: RuntimeReader,
}

/// One live attachment plus the feeds that project it.
pub(crate) struct Session {
    pub(crate) ports: SessionPorts,
    attachment: Attachment,
    cancellation: CancellationToken,
    tasks: JoinSet<()>,
}

impl Session {
    pub(crate) fn connected(&self) -> &phoxal_supervisor_client::Connected {
        self.attachment.connected()
    }

    /// Attach at `endpoint` and start every feed.
    ///
    /// # Errors
    ///
    /// Any direct handshake or supervisor-contract failure.
    pub(crate) async fn open(endpoint: &str, project: String) -> Result<Self> {
        let attachment =
            Attachment::open(&AttachmentConfig::new(endpoint, CLIENT_PARTICIPANT)).await?;
        let epoch = AttachmentEpoch::new(attachment.execution());
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
            attachment: attachment.port(),
            epoch,
            project,
            stores: stores.clone(),
            events: events_tx,
            cancellation: cancellation.clone(),
        };
        let drive = ManualDrive::derive(attachment.connected().manual_drive);
        feeds::spawn_all(&mut tasks, context, input_rx, drive);

        Ok(Self {
            ports: SessionPorts {
                events: AttachmentEvents::new(events_rx),
                supervisor: SupervisorCommands {
                    attachment: attachment.port(),
                },
                input_commands: InputCommands::new(input_tx),
                logs: LogReader::new(stores.logs.clone()),
                runtimes: RuntimeReader::new(stores.runtimes.clone()),
            },
            attachment,
            cancellation,
            tasks,
        })
    }

    /// Observe the highest-revision snapshot this session has installed.
    pub(crate) fn snapshots(
        &self,
    ) -> tokio::sync::watch::Receiver<Option<supervisor::snapshot::Snapshot>> {
        self.attachment.port().snapshots()
    }

    /// The most recent snapshot, without awaiting.
    pub(crate) fn snapshot(&self) -> Option<supervisor::snapshot::Snapshot> {
        self.attachment.port().snapshot()
    }

    /// Resolve when the supervisor's identity token is lost.
    pub(crate) async fn disconnected(&self) {
        self.attachment.port().disconnected().await;
    }

    pub(crate) async fn wait_ready(&self) -> Result<supervisor::snapshot::Snapshot> {
        Ok(self.attachment.port().wait_ready().await?)
    }

    /// One backward page of the supervisor's bounded log history.
    pub(crate) async fn logs(
        &self,
        participant_id: Option<String>,
        limit: u32,
        before_sequence: Option<u64>,
    ) -> Result<logs::Snapshot> {
        Ok(self
            .attachment
            .port()
            .logs(participant_id, limit, before_sequence)
            .await?)
    }

    /// The live log stream that continues a [`Self::logs`] page.
    pub(crate) async fn follow_logs(
        &self,
    ) -> Result<phoxal_bus::StreamReceiver<supervisor::endpoint::logs::FollowEndpoint>> {
        Ok(self.attachment.port().follow_logs().await?)
    }

    /// Detach: stop every feed, then close the session. The daemon is
    /// unaffected - detaching is not stopping.
    pub(crate) async fn shutdown(mut self) {
        self.cancellation.cancel();
        self.tasks.shutdown().await;
        if let Err(error) = self.attachment.close().await {
            tracing::debug!(error = %error, "closing the attachment session failed");
        }
    }
}
