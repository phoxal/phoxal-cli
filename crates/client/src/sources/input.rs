use anyhow::Result;
use phoxal::raw::Bus;
use tokio::sync::mpsc;

use super::TelemetryBackend;
use crate::ports::input::InputCommand;

pub(crate) async fn run(
    bus: Bus,
    telemetry: TelemetryBackend,
    commands: &mut mpsc::Receiver<InputCommand>,
) -> Result<()> {
    super::joypad_devices_feed_loop(bus, &telemetry, commands).await
}
