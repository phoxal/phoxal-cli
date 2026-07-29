use std::convert::Infallible;

use anyhow::Result;
use phoxal::raw::Bus;
use phoxal_cli_observation::RobotScope;

use super::TelemetryBackend;

pub(crate) async fn run(
    bus: Bus,
    scope: RobotScope,
    participants: Vec<String>,
    telemetry: TelemetryBackend,
) -> Result<Infallible> {
    let mut last_capacity_evictions = None;
    super::runtime_performance_feed_loop(
        &bus,
        &scope,
        &participants,
        &telemetry,
        &mut last_capacity_evictions,
    )
    .await
}
