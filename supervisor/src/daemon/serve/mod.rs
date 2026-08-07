//! The supervisor API, served.
//!
//! One session carries every endpoint this daemon owns. They are declared
//! together and run concurrently until the session ends, because they are one
//! contract: a client that can reach `connect` must be able to reach the
//! snapshot, the commands, and the bundle it names.
//!
//! ```text
//! supervisor/connect            query      who this is, and what it speaks
//! supervisor/identity           liveliness token loss IS disconnection
//! supervisor/snapshot           diagnostic every state change, at a new revision
//! supervisor/snapshot/current   query      the same document, on demand
//! supervisor/command            query      restart (fenced) and stop
//! supervisor/bundle/get         query      one file from the finalized bundle
//! supervisor/logs/{snapshot,follow}
//! supervisor/telemetry/{snapshot,follow}
//! ```
//!
//! # NEEDED-FROM-FRAMEWORK
//!
//! `phoxal-bus` can *observe* one exact liveliness key
//! ([`phoxal_bus::Bus::observe_liveliness_key`]) but can only *declare* the
//! participant-shaped token, so the supervisor's identity token is declared
//! through the raw session below. A `Bus::declare_liveliness_key` mirroring the
//! observer would remove the one place this crate reaches past `phoxal-bus`.

pub(crate) mod bundle;
pub(crate) mod command;
pub(crate) mod logs;
pub(crate) mod telemetry;

#[cfg(test)]
mod tests;

use anyhow::{Context, Result, anyhow};
use phoxal_bus::{Bus, Codec, DiagnosticPublisher, MessagePack, QueryFailure, ServerQueryable};
use phoxal_manifest::bundle::BundleResolver;
use phoxal_supervisor_api::{IDENTITY_KEY, RobotApi, SupervisorSchemas, supervisor};
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio_util::sync::CancellationToken;

use super::state::ExecutionState;
use command::Control;

/// Serve every supervisor endpoint until `shutdown` is cancelled.
///
/// The identity token is a local binding rather than a field: it must live for
/// exactly as long as this function does, and losing it is what tells every
/// attached client the execution is gone.
pub(crate) async fn serve(
    bus: Bus,
    state: ExecutionState,
    control: Control,
    resolver: BundleResolver,
    shutdown: CancellationToken,
) -> Result<()> {
    let identity_key = bus.full_key(IDENTITY_KEY);
    let _identity = bus
        .session()
        .liveliness()
        .declare_token(identity_key.clone())
        .await
        .map_err(|error| anyhow!("failed to declare the supervisor identity token: {error}"))?;
    tracing::debug!(key = %identity_key, "supervisor identity token declared");

    let connect = declare(&bus, supervisor::topic::owner().connect().topic().key()).await?;
    let current = declare(&bus, supervisor::topic::owner().snapshot().current().key()).await?;
    let commands = declare(&bus, supervisor::topic::owner().command().topic().key()).await?;
    let files = declare(&bus, supervisor::topic::owner().bundle().get().key()).await?;
    let snapshots = DiagnosticPublisher::<supervisor::snapshot::Update>::new(
        bus.clone(),
        &supervisor::topic::owner().snapshot().topic(),
    )
    .context("failed to declare the supervisor snapshot publisher")?;

    let logs = logs::run(&bus, logs::generation()?);
    let telemetry = telemetry::run(&bus, telemetry::generation()?);

    let published = publish_snapshots(&state, snapshots, shutdown.clone());
    let connect = serve_connect(&bus, &connect, &state);
    let current = serve_current(&bus, &current, &state);
    let commands = command::serve(&bus, &commands, &state, &control);
    let files = bundle::serve(&bus, &files, &resolver);

    tokio::select! {
        () = shutdown.cancelled() => Ok(()),
        () = published => Ok(()),
        () = connect => Ok(()),
        () = current => Ok(()),
        () = commands => Ok(()),
        () = files => Ok(()),
        result = logs => result,
        result = telemetry => result,
    }
}

/// Push every published revision. The snapshot is complete state, so a client
/// that misses one is corrected by the next; the `current` query exists for the
/// one it cannot wait for.
async fn publish_snapshots(
    state: &ExecutionState,
    publisher: DiagnosticPublisher<supervisor::snapshot::Update>,
    shutdown: CancellationToken,
) {
    let mut published = state.subscribe();
    loop {
        let snapshot = published.borrow_and_update().clone();
        if let Err(error) = publisher.publish(supervisor::snapshot::Update::V0(snapshot)) {
            tracing::debug!("failed to publish a supervisor snapshot: {error}");
        }
        tokio::select! {
            () = shutdown.cancelled() => return,
            changed = published.changed() => {
                if changed.is_err() {
                    return;
                }
            }
        }
    }
}

async fn serve_connect(bus: &Bus, queryable: &ServerQueryable, state: &ExecutionState) {
    answer(
        bus,
        queryable,
        "connect",
        |_: supervisor::connect::Request| {
            supervisor::connect::Reply::V0 {
                robot: state.robot(),
                // Not a negotiation: every version here is a one-variant enum, so a
                // client on another train fails to decode this reply and serde's
                // own error names what it found.
                api: RobotApi::V0_1,
                schemas: SupervisorSchemas::current(),
                mode: state.mode(),
            }
        },
    )
    .await;
}

async fn serve_current(bus: &Bus, queryable: &ServerQueryable, state: &ExecutionState) {
    answer(
        bus,
        queryable,
        "snapshot/current",
        |_: supervisor::snapshot::CurrentRequest| {
            supervisor::snapshot::Current::V0(state.snapshot())
        },
    )
    .await;
}

pub(crate) async fn declare(bus: &Bus, key: &str) -> Result<ServerQueryable> {
    bus.declare_server(key)
        .await
        .with_context(|| format!("failed to declare the supervisor server on {key}"))
}

/// Answer one query topic until the session closes.
///
/// A malformed request takes Zenoh's error leg rather than being folded into a
/// successful reply: it is the caller's bug, not an outcome the contract
/// expresses, and a client must be able to tell the two apart.
pub(crate) async fn answer<Request, Reply>(
    bus: &Bus,
    queryable: &ServerQueryable,
    label: &str,
    handle: impl Fn(Request) -> Reply,
) where
    Request: DeserializeOwned,
    Reply: Serialize,
{
    answer_then(bus, queryable, label, move |request| {
        (handle(request), None)
    })
    .await;
}

/// What a handler asks to happen *after* its reply has left this daemon.
///
/// It exists for exactly one case: an accepted `Stop`. Cancelling the
/// execution tears this serving session down, so doing it inside the handler
/// destroys the queryable while the reply is still in flight and the client
/// sees its query stream close with no responder instead of the acceptance it
/// was promised. The effect runs once the reply has been attempted, in either
/// direction: a client that could not be told still asked for a stop.
pub(crate) type AfterReply = Option<Box<dyn FnOnce() + Send>>;

/// [`answer`], plus a post-reply effect the handler chooses per request.
pub(crate) async fn answer_then<Request, Reply>(
    bus: &Bus,
    queryable: &ServerQueryable,
    label: &str,
    handle: impl Fn(Request) -> (Reply, AfterReply),
) where
    Request: DeserializeOwned,
    Reply: Serialize,
{
    loop {
        let query = match queryable.recv().await {
            Ok(query) => query,
            // The bus closed: the session is ending, which is not a fault.
            Err(error) => {
                tracing::debug!("the supervisor {label} server stopped: {error}");
                return;
            }
        };
        let decoded = query
            .request_bytes()
            .map_err(|error| error.to_string())
            .and_then(|bytes| {
                MessagePack::decode::<Request>(&bytes).map_err(|error| error.to_string())
            });
        let request = match decoded {
            Ok(request) => request,
            Err(detail) => {
                let failure =
                    QueryFailure::invalid_argument(format!("malformed {label} request: {detail}"));
                if let Err(error) = query.reply_err(&failure).await {
                    tracing::debug!("failed to reject a malformed {label} request: {error}");
                }
                continue;
            }
        };
        let (reply, after) = handle(request);
        match MessagePack::encode(&reply) {
            Ok(payload) => {
                if let Err(error) = query.reply(bus, payload).await {
                    tracing::debug!("failed to answer a supervisor {label} query: {error}");
                }
            }
            Err(error) => {
                let failure =
                    QueryFailure::internal(format!("failed to encode the {label} reply: {error}"));
                let _ = query.reply_err(&failure).await;
            }
        }
        if let Some(after) = after {
            after();
        }
    }
}
