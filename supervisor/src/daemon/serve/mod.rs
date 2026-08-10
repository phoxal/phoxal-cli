//! Supervisor-owned query and observation endpoints.
//!
//! One endpoint here is unlike the rest: `supervisor/connect` is frozen across
//! every framework line and answers with this daemon's framework train, which
//! is the whole of what the two binaries negotiate. Every other endpoint below
//! assumes that comparison already agreed, so none of them carries a version.

use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use phoxal_api::supervisor;
use phoxal_api::supervisor::command::{Command, CommandOutcome, CommandRejection};
use phoxal_api::supervisor::connect::{ConnectReply, ConnectRequest};
use phoxal_api::supervisor::snapshot::SnapshotDocument;
use phoxal_bundle::RuntimeBundle;
use phoxal_bus::{
    BusHandle, Codec, EndpointDescriptor, IncomingQuery, MessagePack, QueryFailure,
    ServerQueryable, StreamPublisher,
};
use phoxal_model::robot::KinematicConfig;
use phoxal_runtime_contract::version::FrameworkVersion;
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use super::state::ExecutionState;
use crate::process::spec::SupervisorAction;

mod logs;
mod telemetry;

#[derive(Clone)]
pub(crate) struct Control {
    pub(crate) actions: mpsc::Sender<SupervisorAction>,
    pub(crate) stop: CancellationToken,
}

pub(crate) async fn serve(
    bus: BusHandle,
    state: ExecutionState,
    control: Control,
    bundle: RuntimeBundle,
    shutdown: CancellationToken,
) -> Result<()> {
    let mut tasks = JoinSet::new();
    tasks.spawn(serve_connect(bus.clone()));
    tasks.spawn(serve_snapshots(bus.clone(), state.clone()));
    tasks.spawn(serve_current(bus.clone(), state.clone()));
    tasks.spawn(serve_info(bus.clone(), bundle.clone()));
    tasks.spawn(serve_bundle(bus.clone(), bundle.root().to_path_buf()));
    tasks.spawn(serve_commands(bus.clone(), state.clone(), control));
    tasks.spawn(logs::run(bus.clone()));
    tasks.spawn(telemetry::run(bus));

    tokio::select! {
        () = shutdown.cancelled() => {
            tasks.shutdown().await;
            Ok(())
        }
        joined = tasks.join_next() => {
            match joined {
                Some(Ok(Ok(()))) => bail!("a supervisor endpoint task ended before shutdown"),
                Some(Ok(Err(error))) => Err(error),
                Some(Err(error)) => Err(anyhow::anyhow!("a supervisor endpoint task panicked: {error}")),
                None => bail!("all supervisor endpoint tasks ended before shutdown"),
            }
        }
    }
}

/// The frozen attachment bootstrap.
///
/// It answers with this daemon's framework train and nothing else, and it is
/// declared alongside every other endpoint so a client that disagrees learns
/// that from the first thing it asks rather than from a decode failure.
async fn serve_connect(bus: BusHandle) -> Result<()> {
    let server = declare::<supervisor::endpoint::connect::TopicEndpoint>(&bus).await?;
    loop {
        let incoming = server.recv().await?;
        let ConnectRequest::V0 {} = match decode(&incoming).await? {
            Some(request) => request,
            None => continue,
        };
        reply(
            &incoming,
            &bus,
            &ConnectReply::V0 {
                framework: FrameworkVersion::CURRENT,
            },
        )
        .await?;
    }
}

async fn serve_snapshots(bus: BusHandle, state: ExecutionState) -> Result<()> {
    let publisher = StreamPublisher::new(bus, &supervisor::topic::owner().snapshot().topic())?;
    let mut snapshots = state.subscribe();
    publisher.send(SnapshotDocument::V0(snapshots.borrow_and_update().clone()))?;
    loop {
        snapshots
            .changed()
            .await
            .context("the supervisor snapshot authority closed")?;
        publisher.send(SnapshotDocument::V0(snapshots.borrow_and_update().clone()))?;
    }
}

async fn serve_current(bus: BusHandle, state: ExecutionState) -> Result<()> {
    let server = declare::<supervisor::endpoint::snapshot::CurrentEndpoint>(&bus).await?;
    loop {
        let incoming = server.recv().await?;
        let _: supervisor::snapshot::CurrentRequest = match decode(&incoming).await? {
            Some(request) => request,
            None => continue,
        };
        reply(&incoming, &bus, &SnapshotDocument::V0(state.snapshot())).await?;
    }
}

async fn serve_info(bus: BusHandle, bundle: RuntimeBundle) -> Result<()> {
    let server = declare::<supervisor::endpoint::info::TopicEndpoint>(&bus).await?;
    loop {
        let incoming = server.recv().await?;
        let _: supervisor::info::InfoRequest = match decode(&incoming).await? {
            Some(request) => request,
            None => continue,
        };
        reply(
            &incoming,
            &bus,
            &supervisor::info::Info {
                robot: bundle.document().robot_id().clone(),
                clock: bundle.document().robot().clock(),
                manual_drive: manual_drive(bundle.document().robot()),
            },
        )
        .await?;
    }
}

fn manual_drive(robot: &phoxal_model::Robot) -> Option<supervisor::info::ManualDrive> {
    let KinematicConfig::Differential { wheel_base_m, .. } = robot.motion().kinematic() else {
        return None;
    };
    let limits = robot.motion().limits();
    Some(supervisor::info::ManualDrive {
        wheel_base_m: *wheel_base_m,
        max_linear_speed_mps: limits.max_linear_speed_mps,
        max_angular_speed_radps: limits.max_angular_speed_radps,
    })
}

/// Read access to the bundle this daemon is running.
///
/// The daemon is the only process that knows where the bundle lives, so a
/// client asks it for a path instead of reaching into a filesystem it does not
/// own.
async fn serve_bundle(bus: BusHandle, root: PathBuf) -> Result<()> {
    let server = declare::<supervisor::endpoint::bundle::GetEndpoint>(&bus).await?;
    loop {
        let incoming = server.recv().await?;
        let request: supervisor::bundle::GetRequest = match decode(&incoming).await? {
            Some(request) => request,
            None => continue,
        };
        reply(&incoming, &bus, &bundle_entry(&root, &request.path)).await?;
    }
}

/// Resolve one requested path against the bundle root.
///
/// An empty path, an absolute path, and any component that is not a plain name
/// are refused outright: they are requests this endpoint never answers, which
/// is a different answer than an entry the bundle does not have. Everything
/// else joins onto the root, and whether a readable file is there decides the
/// answer.
fn bundle_entry(root: &Path, requested: &str) -> supervisor::bundle::GetResponse {
    let path = Path::new(requested);
    let refusable = requested.is_empty()
        || path.is_absolute()
        || !path
            .components()
            .all(|component| matches!(component, Component::Normal(_)));
    if refusable {
        return supervisor::bundle::GetResponse::InvalidPath;
    }
    let resolved = root.join(path);
    if !resolved.is_file() {
        return supervisor::bundle::GetResponse::Missing;
    }
    std::fs::read(&resolved).map_or(supervisor::bundle::GetResponse::Missing, |bytes| {
        supervisor::bundle::GetResponse::Found { bytes }
    })
}

async fn serve_commands(bus: BusHandle, state: ExecutionState, control: Control) -> Result<()> {
    let server = declare::<supervisor::endpoint::command::TopicEndpoint>(&bus).await?;
    loop {
        let incoming = server.recv().await?;
        let request: supervisor::command::Request = match decode(&incoming).await? {
            Some(request) => request,
            None => continue,
        };
        let supervisor::command::Request::V0 { command: request } = request;
        let (outcome, post_reply) = command(&state, &control, request);
        // Acceptance must reach the client before Stop cancels the endpoint
        // tasks; reversing these operations turns a successful stop into an
        // ambiguous no-responder failure at the caller.
        reply(&incoming, &bus, &supervisor::command::Reply::V0 { outcome }).await?;
        post_reply.apply(&control);
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum PostReply {
    #[default]
    None,
    Stop,
}

impl PostReply {
    fn apply(self, control: &Control) {
        if self == Self::Stop {
            control.stop.cancel();
        }
    }
}

fn command(
    state: &ExecutionState,
    control: &Control,
    command: Command,
) -> (CommandOutcome, PostReply) {
    match command {
        Command::Restart {
            participant,
            expected_producer,
        } => {
            let Some(entry) = state.roster().resolve(&participant).cloned() else {
                return (
                    rejected(CommandRejection::UnknownParticipant),
                    PostReply::None,
                );
            };
            let producer = state
                .snapshot()
                .processes
                .iter()
                .find(|process| process.participant == participant)
                .and_then(|process| process.producer);
            if producer != expected_producer {
                return (rejected(CommandRejection::ProducerFenced), PostReply::None);
            }
            let outcome = match control
                .actions
                .try_send(SupervisorAction::Restart { key: entry.key })
            {
                Ok(()) => accepted_at(state),
                Err(mpsc::error::TrySendError::Full(_)) => rejected(CommandRejection::Busy),
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    rejected(CommandRejection::ControlClosed)
                }
            };
            (outcome, PostReply::None)
        }
        Command::Stop { expected_revision } => {
            let revision = state.snapshot().revision;
            if revision != expected_revision {
                return (rejected(CommandRejection::RevisionStale), PostReply::None);
            }
            (
                CommandOutcome::Accepted {
                    at_revision: revision,
                },
                PostReply::Stop,
            )
        }
        // phoxald supervises one execution, never the machine under it, so the
        // two host operations are refused with the reason that says exactly
        // that rather than silently ignored.
        Command::Reboot { .. } | Command::Poweroff { .. } => (
            rejected(CommandRejection::UnsupportedHostAction),
            PostReply::None,
        ),
    }
}

const fn rejected(reason: CommandRejection) -> CommandOutcome {
    CommandOutcome::Rejected { reason }
}

fn accepted_at(state: &ExecutionState) -> CommandOutcome {
    CommandOutcome::Accepted {
        at_revision: state.snapshot().revision,
    }
}

async fn declare<E: EndpointDescriptor>(bus: &BusHandle) -> Result<ServerQueryable> {
    Ok(bus.declare_server(E::TOPIC).await?)
}

async fn decode<T: serde::de::DeserializeOwned>(incoming: &IncomingQuery) -> Result<Option<T>> {
    match MessagePack::decode(&incoming.request_bytes()?) {
        Ok(request) => Ok(Some(request)),
        Err(error) => {
            incoming
                .reply_err(&QueryFailure::invalid_argument(error.to_string()))
                .await?;
            Ok(None)
        }
    }
}

async fn reply<T: serde::Serialize>(
    incoming: &IncomingQuery,
    bus: &BusHandle,
    response: &T,
) -> Result<()> {
    incoming
        .reply(bus, MessagePack::encode(response)?)
        .await
        .map_err(Into::into)
}

#[cfg(test)]
mod tests;
