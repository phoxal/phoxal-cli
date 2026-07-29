use super::TelemetryBackend;
use anyhow::Result;
use phoxal::bus::{ContractBody, Subscribe, Subscriber, Topic};
use phoxal::raw::Bus;
use phoxal_api::v0_1 as api;
use phoxal_cli_observation::ClockSample;

pub(crate) async fn run(bus: Bus, telemetry: TelemetryBackend) -> Result<()> {
    let topic = Topic::<Subscribe<api::simulation::Clock>>::new_static(
        <api::simulation::Clock as ContractBody>::TOPIC,
    );
    let subscriber = Subscriber::<api::simulation::Clock>::new(&bus, &topic, 32).await?;
    loop {
        let received = subscriber.recv().await?;
        telemetry.record_clock(ClockSample {
            now_ns: received
                .metadata
                .produced_exactly_at()
                .map_or(0, |at| at.ticks()),
            step: received.body.step,
        });
    }
}
