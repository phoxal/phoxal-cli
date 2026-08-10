//! Supervisor-owned query and observation endpoints.

use anyhow::{Context, Result, bail};
use phoxal_bundle::RuntimeBundle;
use phoxal_bus::{
    BusHandle, Codec, EndpointDescriptor, IncomingQuery, MessagePack, QueryFailure,
    ServerQueryable, StreamPublisher,
};
use phoxal_model::robot::KinematicConfig;
use phoxal_supervisor_api::{
    Command, CommandOutcome, CommandRejection, SnapshotDocument, payload, supervisor,
};
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
    tasks.spawn(serve_snapshots(bus.clone(), state.clone()));
    tasks.spawn(serve_current(bus.clone(), state.clone()));
    tasks.spawn(serve_runtime_info(bus.clone(), bundle.clone()));
    tasks.spawn(serve_commands(bus.clone(), state.clone(), control));
    tasks.spawn(logs::run(bus.clone(), state.roster()));
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
        let _: payload::snapshot::CurrentRequest = match decode(&incoming).await? {
            Some(request) => request,
            None => continue,
        };
        reply(&incoming, &bus, &SnapshotDocument::V0(state.snapshot())).await?;
    }
}

async fn serve_runtime_info(bus: BusHandle, bundle: RuntimeBundle) -> Result<()> {
    let server = declare::<supervisor::endpoint::runtime::InfoEndpoint>(&bus).await?;
    loop {
        let incoming = server.recv().await?;
        let _: payload::runtime::InfoRequest = match decode(&incoming).await? {
            Some(request) => request,
            None => continue,
        };
        reply(
            &incoming,
            &bus,
            &payload::runtime::Info {
                robot: bundle.document().robot_id().clone(),
                clock: bundle.document().robot().clock(),
                robot_api: bundle.document().robot_api(),
                manual_drive: manual_drive(bundle.document().robot()),
            },
        )
        .await?;
    }
}

fn manual_drive(robot: &phoxal_model::Robot) -> Option<payload::runtime::ManualDrive> {
    let KinematicConfig::Differential { wheel_base_m, .. } = robot.motion().kinematic() else {
        return None;
    };
    let limits = robot.motion().limits();
    Some(payload::runtime::ManualDrive {
        wheel_base_m: *wheel_base_m,
        max_linear_speed_mps: limits.max_linear_speed_mps,
        max_angular_speed_radps: limits.max_angular_speed_radps,
    })
}

async fn serve_commands(bus: BusHandle, state: ExecutionState, control: Control) -> Result<()> {
    let server = declare::<supervisor::endpoint::control::TopicEndpoint>(&bus).await?;
    loop {
        let incoming = server.recv().await?;
        let request: payload::command::Request = match decode(&incoming).await? {
            Some(request) => request,
            None => continue,
        };
        let payload::command::Request::V0 { command: request } = request;
        let (outcome, post_reply) = command(&state, &control, request);
        // Acceptance must reach the client before Stop cancels the endpoint
        // tasks; reversing these operations turns a successful stop into an
        // ambiguous no-responder failure at the caller.
        reply(&incoming, &bus, &payload::command::Reply::V0 { outcome }).await?;
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
        Command::Stop => (accepted_at(state), PostReply::Stop),
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
