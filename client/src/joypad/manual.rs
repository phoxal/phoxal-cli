//! Turning a robot's authored model into manual-drive parameters.
//!
//! This used to travel on the supervisor snapshot: the daemon derived it and
//! republished a derived copy on every revision. It does not any more
//! (organization#978). The pad is attached to the machine running this client,
//! so the client is the only consumer, and the kinematics it scales against
//! come from the finalized `robot.yaml` this client fetches once through
//! `bundle/get` after attaching - the one document that owns them.
//!
//! A robot that cannot be driven manually is not an error. It yields a typed
//! [`ManualDriveUnsupported`] the UI renders, rather than an empty device list
//! or a sentence composed on another machine.

use phoxal_cli_observation::ManualDriveUnsupported;
use phoxal_manifest::source::robot::v0::{KinematicConfig, RobotSection};

/// Parameters that turn a normalized trigger deflection into a physical
/// differential-drive command.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct ManualDrive {
    pub(crate) wheel_base_m: f64,
    /// The speed one differential side reaches at full trigger, already
    /// clamped by both the linear and the angular authored limit.
    pub(crate) side_speed_mps: f64,
}

impl ManualDrive {
    /// Derive the parameters from the authored robot section of a finalized
    /// `robot.yaml`.
    ///
    /// # Errors
    ///
    /// The typed reason this robot rules manual input out.
    pub(crate) fn derive(robot: &RobotSection) -> Result<Self, ManualDriveUnsupported> {
        let KinematicConfig::Differential { wheel_base_m, .. } = &robot.kinematic else {
            return Err(ManualDriveUnsupported::NoDifferentialBase);
        };
        let wheel_base_m = *wheel_base_m;
        if !(wheel_base_m.is_finite() && wheel_base_m > 0.0) {
            return Err(ManualDriveUnsupported::UnusableWheelBase);
        }
        let limits = robot
            .motion_limits
            .validate()
            .map_err(|_| ManualDriveUnsupported::MissingMotionLimits)?;
        Ok(Self {
            wheel_base_m,
            // Whichever authored limit binds first is the one that decides
            // what a full deflection means, so a generous angular limit can
            // never raise the side speed past what the robot may travel.
            side_speed_mps: limits
                .max_linear_speed_mps
                .min(limits.max_angular_speed_radps * wheel_base_m / 2.0),
        })
    }
}
