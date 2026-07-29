use phoxal_cli_core::session::JoypadDevicesSample;
pub use phoxal_cli_core::session::MotionSample as MotionObservation;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct InputObservation {
    pub joypads: JoypadDevicesSample,
    pub motion: Option<MotionObservation>,
}
