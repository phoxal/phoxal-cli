use std::convert::Infallible;

use anyhow::Result;
use phoxal_bus::Bus;
use phoxal_cli_observation::RobotScope;

use super::TelemetryBackend;

pub(crate) async fn run(
    bus: Bus,
    scope: RobotScope,
    telemetry: TelemetryBackend,
) -> Result<Infallible> {
    super::router_metrics_feed_loop(&bus, &scope, &telemetry).await
}
