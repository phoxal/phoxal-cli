use anyhow::Result;
use phoxal::bus::{ContractBody, Subscribe, Subscriber, Topic};
use phoxal::raw::Bus;
use phoxal_api::v0_1 as api;
use phoxal_cli_observation::ClockSample;
use tokio::sync::mpsc;

use super::TelemetryUpdate;

pub(crate) async fn run(bus: Bus, updates: mpsc::UnboundedSender<TelemetryUpdate>) -> Result<()> {
    let topic = Topic::<Subscribe<api::simulation::Clock>>::new_static(
        <api::simulation::Clock as ContractBody>::TOPIC,
    );
    let subscriber = Subscriber::<api::simulation::Clock>::new(&bus, &topic, 32).await?;
    loop {
        let received = subscriber.recv().await?;
        let _ = updates.send(TelemetryUpdate::Clock(ClockSample {
            now_ns: received
                .metadata
                .produced_exactly_at()
                .map_or(0, |at| at.ticks()),
            step: received.body.step,
        }));
    }
}
