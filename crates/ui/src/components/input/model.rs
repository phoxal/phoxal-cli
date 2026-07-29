use phoxal_cli_observation::InputObservation;

use crate::app::DeviceId;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct InputModel {
    pub observation: Option<InputObservation>,
    pub candidate: Option<DeviceId>,
    pub pending_selection: Option<DeviceId>,
    pub pending_enabled: Option<bool>,
}

impl InputModel {
    pub fn reconcile_authoritative(&mut self, observation: InputObservation) {
        let available = observation.joypads.available.as_ref();
        if self
            .candidate
            .as_ref()
            .is_none_or(|candidate| !available.iter().any(|device| device.id == candidate.0))
        {
            self.candidate = available.first().map(|device| DeviceId(device.id.clone()));
        }
        if self
            .pending_selection
            .as_ref()
            .is_some_and(|pending| observation.joypads.selected.as_deref() == Some(&pending.0))
        {
            self.pending_selection = None;
        }
        if self
            .pending_enabled
            .is_some_and(|pending| observation.joypads.enabled == pending)
        {
            self.pending_enabled = None;
        }
        self.observation = Some(observation);
    }
}
