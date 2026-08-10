//! Manual input: read the operator's gamepad, publish `motion::ManualCommand`.
//!
//! Unlike its sibling sources this one does not ingest from the bus - the pad
//! is attached to the machine running this client, so the device state is
//! produced here and only the resulting command goes out (see
//! [`crate::joypad`] for the operator-side ownership contract).

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use phoxal_api::v0_1 as api;
use phoxal_bus::SetpointPublisher;
use phoxal_cli_observation::{
    AttachmentEvent, ManualDriveUnsupported, ObservationSource, SourceStatus,
};
use tokio::sync::mpsc;

use super::FeedContext;
use crate::attach::ports::input::InputCommand;
use crate::joypad::manual::ManualDrive;
use crate::joypad::{Joypad, STOP_REPEAT_COUNT};

const SOURCE: ObservationSource = ObservationSource::Input;

/// How often the selected pad is read and a command published.
const POLL_HZ: f64 = 50.0;

/// Read the local gamepad until the command port closes.
pub(crate) async fn run(
    context: FeedContext,
    mut commands: mpsc::Receiver<InputCommand>,
    drive: Result<ManualDrive, ManualDriveUnsupported>,
) {
    let commands = &mut commands;
    super::until_cancelled(&context, SOURCE, feed(&context, commands, drive)).await;
}

async fn feed(
    context: &FeedContext,
    commands: &mut mpsc::Receiver<InputCommand>,
    drive: Result<ManualDrive, ManualDriveUnsupported>,
) -> Result<()> {
    let publisher = SetpointPublisher::new(
        context.attachment.bus().clone(),
        &api::topic::client().motion().manual(),
    )
    .context("failed to attach the manual command publisher")?;

    let mut joypad = Joypad::open(drive.ok(), drive.err());
    publish_joypad(context, joypad.sample()).await?;
    context.health(SOURCE, SourceStatus::Live).await;

    let mut ticker = tokio::time::interval(Duration::from_secs_f64(1.0 / POLL_HZ));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // A revoked authority owes the robot a stop. That stop must survive a
    // single dropped publish, so it is a countdown drained a tick at a time
    // rather than one best-effort send.
    let mut pending_stops = 0_usize;
    let mut dropped_commands = 0_u64;

    loop {
        tokio::select! {
            _ = ticker.tick() => {
                publish_stop(&publisher, &mut pending_stops, &mut dropped_commands);

                let polled = joypad.poll();
                let (command, derived) = joypad.command();
                if polled.zero_required || derived.zero_required {
                    queue_stop(&mut pending_stops);
                    publish_stop(&publisher, &mut pending_stops, &mut dropped_commands);
                }
                if polled.changed || derived.changed {
                    publish_joypad(context, joypad.sample()).await?;
                }
                if let Some(command) = command
                    && publisher.send(command).is_err()
                {
                    dropped_commands = dropped_commands.saturating_add(1);
                }
            }
            command = commands.recv() => {
                let Some(command) = command else {
                    // The typed port closed: this attachment is ending. Leave
                    // the robot stopped rather than at the operator's last
                    // input.
                    publish_stop_repeats(&publisher);
                    return Ok(());
                };
                let zero_required = match command {
                    InputCommand::Select(id) => joypad.select(&id),
                    InputCommand::SetEnabled(enabled) => joypad.set_enabled(enabled),
                    InputCommand::Rescan => joypad.rescan(),
                };
                if zero_required {
                    queue_stop(&mut pending_stops);
                    publish_stop(&publisher, &mut pending_stops, &mut dropped_commands);
                }
                // Re-observing IS the acknowledgement, including for a request
                // the registry rejected.
                publish_joypad(context, joypad.sample()).await?;
            }
        }
    }
}

/// Publish one device inventory as an input observation.
///
/// It takes the sample by value rather than the pad: the `gilrs` handle is
/// neither `Send` nor `Sync`, so holding a reference to it across an await
/// would make this whole feed unspawnable.
async fn publish_joypad(
    context: &FeedContext,
    sample: phoxal_cli_observation::JoypadDevicesSample,
) -> Result<()> {
    let motion = context.stores.motion.read().await.current();
    let observation = context
        .stores
        .input
        .write()
        .await
        .record_joypads(sample, motion);
    context
        .events
        .send(AttachmentEvent::InputChanged {
            epoch: context.epoch,
            values: Arc::new(observation),
        })
        .await?;
    Ok(())
}

fn queue_stop(pending_stops: &mut usize) {
    *pending_stops = (*pending_stops).max(STOP_REPEAT_COUNT);
}

/// Send one queued stop, keeping the countdown intact if the publish failed so
/// the next tick tries again.
fn publish_stop(
    publisher: &SetpointPublisher<api::endpoint::motion::ManualEndpoint>,
    pending_stops: &mut usize,
    dropped: &mut u64,
) {
    if *pending_stops == 0 {
        return;
    }
    if publisher.send(stop()).is_err() {
        *dropped = dropped.saturating_add(1);
    } else {
        *pending_stops -= 1;
    }
}

/// Drain the whole stop budget at once, for an exit that has no next tick.
fn publish_stop_repeats(publisher: &SetpointPublisher<api::endpoint::motion::ManualEndpoint>) {
    for attempt in 0..STOP_REPEAT_COUNT {
        if let Err(error) = publisher.send(stop()) {
            tracing::warn!(
                attempt = attempt + 1,
                error = %error,
                "failed to publish a manual stop while detaching"
            );
        }
    }
}

const fn stop() -> api::motion::ManualCommand {
    api::motion::ManualCommand {
        linear_x_mps: 0.0,
        angular_z_radps: 0.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use phoxal_bus::{BusConfig, BusOwner, SetpointReceiver};
    use phoxal_runtime_contract::identity::ExecutionId;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_queued_stop_reaches_the_manual_contract() {
        let (owner, bus) = BusOwner::open(BusConfig::for_external(
            ExecutionId::mint(),
            None,
            Vec::new(),
        ))
        .await
        .expect("bus should open");
        let publisher =
            SetpointPublisher::new(bus.clone(), &api::topic::client().motion().manual())
                .expect("publisher attaches");
        let subscriber = SetpointReceiver::new(&bus, &api::topic::owner().motion().manual())
            .await
            .expect("subscriber attaches");

        let mut pending_stops = 0;
        let mut dropped = 0;
        queue_stop(&mut pending_stops);
        publish_stop(&publisher, &mut pending_stops, &mut dropped);

        assert_eq!(dropped, 0);
        assert_eq!(pending_stops, STOP_REPEAT_COUNT - 1);
        let received = tokio::time::timeout(Duration::from_secs(2), subscriber.recv())
            .await
            .expect("the stop should arrive")
            .expect("the stop should decode");
        assert_eq!(received.body.linear_x_mps, 0.0);
        assert_eq!(received.body.angular_z_radps, 0.0);
        owner.close().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn a_failed_stop_keeps_its_place_in_the_budget() {
        // A stop lost to a closed bus must not be counted as delivered, or a
        // single transport hiccup would leave the robot moving.
        let (owner, bus) = BusOwner::open(BusConfig::for_external(
            ExecutionId::mint(),
            None,
            Vec::new(),
        ))
        .await
        .expect("bus should open");
        let publisher =
            SetpointPublisher::new(bus.clone(), &api::topic::client().motion().manual())
                .expect("publisher attaches");
        owner.close().await;

        let mut pending_stops = STOP_REPEAT_COUNT;
        let mut dropped = 0;
        publish_stop(&publisher, &mut pending_stops, &mut dropped);

        assert_eq!(pending_stops, STOP_REPEAT_COUNT, "the stop is still owed");
        assert_eq!(dropped, 1);
    }
}
