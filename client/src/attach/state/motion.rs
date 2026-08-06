use phoxal_cli_observation::MotionObservation;

#[derive(Default)]
pub(crate) struct MotionStore(pub MotionObservation);

impl MotionStore {
    pub fn record(&mut self, motion: MotionObservation) -> MotionObservation {
        self.0 = motion;
        self.0
    }

    pub fn current(&self) -> MotionObservation {
        self.0
    }

    pub fn clear(&mut self) {
        self.0 = MotionObservation::default();
    }
}
