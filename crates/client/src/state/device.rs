use phoxal_cli_observation::DeviceObservation;

#[derive(Default)]
pub(crate) struct DeviceStore(pub DeviceObservation);

impl DeviceStore {
    pub fn record(
        &mut self,
        robot: phoxal_cli_core::runtime::RobotKey,
        sample: phoxal_cli_observation::DeviceSample,
    ) -> DeviceObservation {
        self.0.robots.insert(robot, sample);
        self.0.clone()
    }

    pub fn record_clock(
        &mut self,
        sample: phoxal_cli_observation::ClockSample,
    ) -> DeviceObservation {
        self.0.clock = Some(sample);
        self.0.clone()
    }

    pub fn clear(&mut self) {
        self.0 = DeviceObservation::default();
    }
}
