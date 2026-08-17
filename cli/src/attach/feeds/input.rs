//! Manual input: read the operator's gamepad, publish `motion::ManualCommand`.
//!
//! Unlike its sibling sources this one does not ingest from the bus - the pad
//! is attached to the machine running this client, so the device state is
//! produced here and only the resulting command goes out (see
//! [`crate::joypad`] for the operator-side ownership contract).

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use phoxal_cli_observation::{AttachmentEvent, ObservationSource, SourceStatus};
use phoxal_client::SetpointPublisher;
use phoxal_client::robot as api;
use tokio::sync::mpsc;

use super::FeedContext;
use crate::attach::ports::input::InputCommand;
use crate::joypad::{Joypad, STOP_REPEAT_COUNT};

const SOURCE: ObservationSource = ObservationSource::Input;

/// How often the selected pad is read and a command published.
const POLL_HZ: f64 = 50.0;

/// Read the local gamepad until the command port closes.
pub(crate) async fn run(context: FeedContext, mut commands: mpsc::Receiver<InputCommand>) {
    let commands = &mut commands;
    super::until_cancelled(&context, SOURCE, feed(&context, commands)).await;
}

async fn feed(context: &FeedContext, commands: &mut mpsc::Receiver<InputCommand>) -> Result<()> {
    let publisher = context
        .client
        .setpoint_publisher(api::topic::client().motion().manual())
        .context("failed to attach the manual command publisher")?;

    let mut joypad = Joypad::open();
    publish_joypad(context, joypad.sample()).await?;
    context.health(SOURCE, SourceStatus::Live).await;

    let mut ticker = tokio::time::interval(Duration::from_secs_f64(1.0 / POLL_HZ));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // A revoked authority owes the robot a stop. That stop must survive a
    // single dropped publish, so it is a countdown drained a tick at a time
    // rather than one best-effort send.
    let mut pending_stops = 0_usize;

    loop {
        tokio::select! {
            _ = ticker.tick() => {
                publish_stop(&publisher, &mut pending_stops);

                let polled = joypad.poll();
                let (command, derived) = joypad.command();
                if polled.zero_required || derived.zero_required {
                    queue_stop(&mut pending_stops);
                    publish_stop(&publisher, &mut pending_stops);
                }
                if polled.changed || derived.changed {
                    publish_joypad(context, joypad.sample()).await?;
                }
                if let Some(command) = command {
                    // A dropped setpoint is replaced by the next tick's, 20ms
                    // later; only a stop owes a retry, and it has one below.
                    let _ = publisher.send(command);
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
                    publish_stop(&publisher, &mut pending_stops);
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
) {
    publish_stop_with(pending_stops, |command| publisher.send(command).is_ok());
}

/// Apply one stop-publish result to the retry budget. The closure keeps the
/// countdown rule independently testable without constructing a live bus.
fn publish_stop_with(
    pending_stops: &mut usize,
    mut publish: impl FnMut(api::motion::ManualCommand) -> bool,
) {
    if *pending_stops == 0 {
        return;
    }
    if publish(stop()) {
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
        linear: 0.0,
        angular: 0.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn successful_stop_publication_decrements_the_retry_budget() {
        let mut pending_stops = STOP_REPEAT_COUNT;
        let mut published = Vec::new();

        publish_stop_with(&mut pending_stops, |command| {
            published.push(command);
            true
        });

        assert_eq!(pending_stops, STOP_REPEAT_COUNT - 1);
        assert_eq!(published.len(), 1);
        assert_eq!(published[0].linear, 0.0);
        assert_eq!(published[0].angular, 0.0);
    }

    #[test]
    fn failed_stop_publication_retains_the_budget_for_the_next_attempt() {
        let mut pending_stops = STOP_REPEAT_COUNT;
        let mut outcomes = [false, true].into_iter();

        publish_stop_with(&mut pending_stops, |_| {
            outcomes.next().expect("an outcome for the first attempt")
        });
        assert_eq!(pending_stops, STOP_REPEAT_COUNT);

        publish_stop_with(&mut pending_stops, |_| {
            outcomes.next().expect("an outcome for the retry")
        });
        assert_eq!(pending_stops, STOP_REPEAT_COUNT - 1);
    }
}
