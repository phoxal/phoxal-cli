//! The supervisor's own presence on each robot bus.
//!
//! One session per robot target, carrying everything the supervisor does on
//! that bus: observing participant Liveliness for the board, and answering the
//! contracts it owns after absorbing the tools that used to serve them
//! (organization#978).
//!
//! One session rather than one per concern - each absorbed contract would
//! otherwise add another connection to the same router for no benefit.

pub(crate) mod assets;
pub(crate) mod logs;
pub(crate) mod telemetry;

use crate::SupervisorState;
use anyhow::{Result, anyhow};
use phoxal_bus::{Bus, BusConfig};
use phoxal_cli_core::identity::ExecutionId;
use phoxal_model::ParticipantAssetResolver;
use std::time::Duration;
use tokio::task::JoinHandle;

/// Open the supervisor's own session on one robot bus.
///
/// It carries everything the supervisor itself does on that bus: observing
/// every planned participant's stable Zenoh Liveliness key, and answering the
/// contracts the supervisor owns (`supervisor/asset`, `supervisor/log`, `supervisor/telemetry`). One session per
/// robot target rather than one per concern - each absorbed contract otherwise
/// adds another connection to the same router for no benefit
/// (organization#978).
///
/// Callers register the finite participant set on the board before starting
/// this; Liveliness traffic for any other key is deliberately ignored. History
/// is enabled by the framework wrapper, so participants that completed setup
/// before this session connected are discovered immediately.
pub fn start_supervisor_session(
    namespace: String,
    robot_id: String,
    connect: String,
    execution: ExecutionId,
    board: SupervisorState,
    assets: ParticipantAssetResolver,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            match supervisor_session_loop(
                namespace.clone(),
                robot_id.clone(),
                connect.clone(),
                execution,
                board.clone(),
                assets.clone(),
            )
            .await
            {
                Ok(()) => break,
                Err(error) => {
                    tracing::debug!("supervisor session waiting for router: {error:#}");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
        }
    })
}

pub(crate) async fn supervisor_session_loop(
    namespace: String,
    robot_id: String,
    connect: String,
    execution: ExecutionId,
    board: SupervisorState,
    assets: ParticipantAssetResolver,
) -> Result<()> {
    let bus = Bus::open(BusConfig {
        participant: super::state::readiness::SUPERVISOR_SESSION_ID.to_string(),
        execution,
        connect_endpoints: vec![connect],
    })
    .await
    .map_err(|error| anyhow!("failed to open bus Liveliness observer: {error}"))?;
    let _observer = bus
        .observe_participant_liveliness(move |event| {
            super::state::readiness::apply_liveliness_event(&board, &namespace, &robot_id, event);
        })
        .await
        .map_err(|error| anyhow!("failed to observe participant Liveliness: {error}"))?;
    // Once declared, the Bus session and Zenoh subscriber own transparent
    // transport reconnection. The outer loop above retries only initial open
    // or declaration failures; there is no application-level heartbeat loop.
    //
    // Serving assets is what parks this task: it returns only when the bus
    // closes, which is also the one condition that used to end the wait here.
    // Everything the supervisor answers on this bus runs here, concurrently,
    // and the session ends when the bus closes - which is what ends both.
    let assets = assets::run(&bus, &assets);
    let logs = logs::run(&bus, logs::generation()?);
    let telemetry = telemetry::run(&bus, telemetry::generation()?);
    let (assets, logs, telemetry) = tokio::join!(assets, logs, telemetry);
    assets?;
    logs?;
    telemetry
}
