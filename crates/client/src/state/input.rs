use phoxal_cli_core::session::JoypadDevicesSample;
use phoxal_cli_observation::{InputObservation, MotionObservation};

#[derive(Default)]
pub(crate) struct InputStore {
    joypads: JoypadDevicesSample,
}

impl InputStore {
    pub fn record_joypads(
        &mut self,
        joypads: JoypadDevicesSample,
        motion: MotionObservation,
    ) -> InputObservation {
        self.joypads = joypads;
        self.observe(motion)
    }

    pub fn observe(&self, motion: MotionObservation) -> InputObservation {
        InputObservation {
            joypads: self.joypads.clone(),
            motion: Some(motion),
        }
    }

    pub fn clear(&mut self) {
        self.joypads = JoypadDevicesSample::default();
    }
}
