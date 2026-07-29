use std::collections::BTreeMap;

use phoxal_cli_core::session::{ClockSample, DeviceSample, RobotKey};

#[derive(Debug, Clone, Default, PartialEq)]
pub struct DeviceObservation {
    pub robots: BTreeMap<RobotKey, DeviceSample>,
    pub clock: Option<ClockSample>,
}
