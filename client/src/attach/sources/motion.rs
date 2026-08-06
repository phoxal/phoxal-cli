use anyhow::Result;
use phoxal_bus::Bus;

use super::TelemetryBackend;

pub(crate) async fn run(bus: Bus, telemetry: TelemetryBackend) -> Result<()> {
    super::control_state_feed_loop(bus, &telemetry).await
}
