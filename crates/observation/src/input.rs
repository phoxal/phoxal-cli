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
    pub selected: Option<String>,
    pub enabled: bool,
    /// Why this robot cannot be driven manually at all. It is a property of
    /// the robot model, not of the operator's hardware, so it is a closed set
    /// the renderer matches on rather than a sentence the daemon composed
    /// (organization#978).
    pub unsupported: Option<ManualDriveUnsupported>,
    pub last_error: Option<String>,
}

/// Why a robot's authored model rules manual driving out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManualDriveUnsupported {
    /// The robot declares no differential base to steer.
    NoDifferentialBase,
    /// `kinematic.wheel_base_m` is absent, non-finite, or not positive.
    UnusableWheelBase,
    /// The robot authors no linear or angular speed limit, so there is no
    /// speed a full deflection could mean.
    MissingMotionLimits,
}

impl std::fmt::Display for ManualDriveUnsupported {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::NoDifferentialBase => "this robot declares no differential base to steer",
            Self::UnusableWheelBase => {
                "robot.kinematic.wheel_base_m must be finite and greater than zero"
            }
            Self::MissingMotionLimits => {
                "this robot authors no linear or angular speed limit to scale input against"
            }
        })
    }
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
