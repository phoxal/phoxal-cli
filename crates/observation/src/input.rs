use std::sync::Arc;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct JoypadDevice {
    pub id: String,
    pub name: String,
    pub status: JoypadDeviceStatus,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum JoypadDeviceStatus {
    Ready,
    Disconnected,
    Unsupported,
    #[default]
    Unknown,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct JoypadDevicesSample {
    pub available: Arc<Vec<JoypadDevice>>,
    pub devices_truncated: usize,
    pub selected: Option<String>,
    pub enabled: bool,
    pub unavailable_reason: Option<String>,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct MotionObservation {
    pub linear_x_mps: f32,
    pub angular_z_radps: f32,
}

pub type MotionSample = MotionObservation;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct InputObservation {
    pub joypads: JoypadDevicesSample,
    pub motion: Option<MotionObservation>,
}
