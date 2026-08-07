//! The disposable attachment session.
//!
//! One [`Session`] is one attachment to one execution: it opens a
//! [`phoxal_supervisor_client::Attachment`], fetches the finalized `robot.yaml`
//! through `bundle/get` to verify identity and derive the client's own manual
//! drive parameters, and runs the feeds the terminal application renders.
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

use std::sync::Arc;

use anyhow::{Context, Result};
use phoxal_cli_observation::{AttachmentEpoch, AttachmentEvent, ConnectionObservation};
use phoxal_runtime_contract::ProducerId;
use phoxal_supervisor_api::{BundleGetOutcome, Command, CommandOutcome, ProcessKey};
use phoxal_supervisor_client::{Attachment, AttachmentConfig};
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

/// The finalized `robot.yaml` this client fetched, already parsed.
#[derive(Debug, Clone)]
pub(crate) struct AttachedManifest {
    pub(crate) manifest: phoxal_manifest::source::robot::Manifest,
}

impl AttachedManifest {
    /// Fetch, parse, and identity-check the execution's finalized manifest.
    ///
    /// The daemon serves the exact bytes it is executing, so this is the one
    /// place the client learns the robot's authored model - remotely as well
    /// as locally, with no local file involved.
    async fn fetch(attachment: &Attachment) -> Result<Self> {
        let bytes = match attachment
            .bundle_file("robot.yaml")
            .await
            .context("failed to read robot.yaml from the running bundle")?
        {
            BundleGetOutcome::Found { bytes } => bytes,
            BundleGetOutcome::Missing => {
                anyhow::bail!("the running bundle serves no robot.yaml")
            }
            BundleGetOutcome::InvalidPath { reason } => {
                anyhow::bail!("the supervisor refused to read robot.yaml: {reason:?}")
            }
            BundleGetOutcome::TooLarge {
                size_bytes,
                limit_bytes,
            } => anyhow::bail!(
                "the running bundle's robot.yaml is {size_bytes} bytes; the supervisor serves at \
                 most {limit_bytes}"
            ),
        };
        let text = String::from_utf8(bytes)
            .context("the running bundle's robot.yaml is not valid UTF-8")?;
        let manifest = phoxal_manifest::source::robot::parse_from_string(&text)
            .context("failed to parse the running bundle's robot.yaml")?;
        let phoxal_manifest::source::robot::Manifest::V0(body) = &manifest;
        // A finalized manifest has `extends` resolved. One that still declares
        // a parent is not the document the daemon claims to be executing.
        anyhow::ensure!(
            body.extends.is_empty(),
            "the running bundle's robot.yaml still declares `extends`; it is not finalized"
        );
        let connected = attachment.connected();
        anyhow::ensure!(
            body.robot.id == connected.robot.id.as_str()
                && body.robot.namespace == connected.robot.namespace.as_str(),
            "the running bundle's robot.yaml declares {}/{}, but the supervisor connected as {}/{}",
            body.robot.namespace,
            body.robot.id,
            connected.robot.namespace,
            connected.robot.id
        );
        Ok(Self { manifest })
    }

    pub(crate) fn robot(&self) -> &phoxal_manifest::source::robot::v0::RobotSection {
        let phoxal_manifest::source::robot::Manifest::V0(body) = &self.manifest;
        &body.robot
    }
}

/// A command channel to the attached supervisor.
///
/// It is a handle over the shared [`Attachment`], not a second connection: the
/// supervisor API is one query topic, so there is nothing to open twice.
#[derive(Clone)]
pub(crate) struct SupervisorCommands {
    attachment: Arc<Attachment>,
}

impl SupervisorCommands {
    /// Restart one process, fenced on the producer the caller last observed.
    pub(crate) async fn restart(
        &self,
        process: ProcessKey,
        expected_producer: ProducerId,
    ) -> Result<CommandOutcome> {
        Ok(self.attachment.restart(process, expected_producer).await?)
    }

    /// End the execution.
    pub(crate) async fn stop(&self) -> Result<CommandOutcome> {
        Ok(self.attachment.command(Command::Stop).await?)
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
    attachment: Arc<Attachment>,
    cancellation: CancellationToken,
    tasks: JoinSet<()>,
}

impl Session {
    /// Attach at `endpoint` and start every feed.
    ///
    /// # Errors
    ///
    /// Any handshake failure, or a running bundle whose manifest does not
    /// match the identity the supervisor connected as.
    pub(crate) async fn open(endpoint: &str, project: String) -> Result<Self> {
        let attachment =
            Arc::new(Attachment::open(&AttachmentConfig::new(endpoint, CLIENT_PARTICIPANT)).await?);
        let manifest = AttachedManifest::fetch(&attachment).await?;
        let epoch = AttachmentEpoch::new(attachment.execution());
        let stores = Stores::new(epoch);
        let (events_tx, events_rx) = mpsc::channel(EVENT_CAPACITY);
        let (input_tx, input_rx) = mpsc::channel(INPUT_CAPACITY);

        events_tx
            .send(AttachmentEvent::EpochChanged(epoch))
            .await
            .expect("the event receiver is alive");
        events_tx
            .send(AttachmentEvent::ConnectionChanged(
                ConnectionObservation::Connected,
            ))
            .await
            .expect("the event receiver is alive");

        let cancellation = CancellationToken::new();
        let mut tasks = JoinSet::new();
        let context = feeds::FeedContext {
            attachment: attachment.clone(),
            epoch,
            project,
            stores: stores.clone(),
            events: events_tx,
            cancellation: cancellation.clone(),
        };
        let drive = ManualDrive::derive(manifest.robot());
        feeds::spawn_all(&mut tasks, context, input_rx, drive);

        Ok(Self {
            ports: SessionPorts {
                events: AttachmentEvents::new(events_rx),
                supervisor: SupervisorCommands {
                    attachment: attachment.clone(),
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
    ) -> tokio::sync::watch::Receiver<Option<phoxal_supervisor_api::Snapshot>> {
        self.attachment.snapshots()
    }

    /// The most recent snapshot, without awaiting.
    pub(crate) fn snapshot(&self) -> Option<phoxal_supervisor_api::Snapshot> {
        self.attachment.snapshot()
    }

    /// Resolve when the supervisor's identity token is lost.
    pub(crate) async fn disconnected(&self) {
        self.attachment.disconnected().await;
    }

    /// End the execution.
    pub(crate) async fn stop(&self) -> Result<CommandOutcome> {
        Ok(self.attachment.command(Command::Stop).await?)
    }

    /// One backward page of the supervisor's bounded log history.
    pub(crate) async fn logs(
        &self,
        participant: Option<phoxal_supervisor_api::Name>,
        limit: u32,
        before_sequence: Option<u64>,
    ) -> Result<phoxal_supervisor_api::supervisor::logs::Snapshot> {
        Ok(self
            .attachment
            .logs(participant, limit, before_sequence)
            .await?)
    }

    /// The live log stream that continues a [`Self::logs`] page.
    pub(crate) async fn follow_logs(
        &self,
    ) -> Result<phoxal_bus::Subscriber<phoxal_supervisor_api::supervisor::logs::Follow>> {
        Ok(self.attachment.follow_logs().await?)
    }

    /// Detach: stop every feed, then close the session. The daemon is
    /// unaffected - detaching is not stopping.
    pub(crate) async fn shutdown(mut self) {
        self.cancellation.cancel();
        self.tasks.shutdown().await;
        // The attachment is shared with the command port the UI may still
        // hold; whichever handle is last closes the session on drop.
        if let Some(attachment) = Arc::into_inner(self.attachment)
            && let Err(error) = attachment.close().await
        {
            tracing::debug!(error = %error, "closing the attachment session failed");
        }
    }
}
