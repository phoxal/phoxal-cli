//! Turning the supervisor's immutable runtime facts into manual-drive parameters.
//!
//! A robot that cannot be driven manually is not an error. It yields a typed
//! [`ManualDriveUnsupported`] the UI renders, rather than an empty device list
//! or a sentence composed on another machine.

use phoxal_cli_observation::ManualDriveUnsupported;
use phoxal_client::supervisor::info;

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
    /// Derive parameters from the canonical runtime facts advertised at attach.
    ///
    /// # Errors
    ///
    /// The typed reason this robot rules manual input out.
    pub(crate) fn derive(drive: Option<info::ManualDrive>) -> Result<Self, ManualDriveUnsupported> {
        let Some(drive) = drive else {
            return Err(ManualDriveUnsupported::NoDifferentialBase);
        };
        let wheel_base_m = drive.wheel_base_m;
        if !(wheel_base_m.is_finite() && wheel_base_m > 0.0) {
            return Err(ManualDriveUnsupported::UnusableWheelBase);
        }
        if !(drive.max_linear_speed_mps.is_finite()
            && drive.max_linear_speed_mps > 0.0
            && drive.max_angular_speed_radps.is_finite()
            && drive.max_angular_speed_radps > 0.0)
        {
            return Err(ManualDriveUnsupported::MissingMotionLimits);
        }
        Ok(Self {
            wheel_base_m,
            // Whichever authored limit binds first is the one that decides
            // what a full deflection means, so a generous angular limit can
            // never raise the side speed past what the robot may travel.
            side_speed_mps: drive
                .max_linear_speed_mps
                .min(drive.max_angular_speed_radps * wheel_base_m / 2.0),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drive(wheel_base_m: f64, linear: f64, angular: f64) -> info::ManualDrive {
        info::ManualDrive {
            wheel_base_m,
            max_linear_speed_mps: linear,
            max_angular_speed_radps: angular,
        }
    }

    /// Whichever authored limit binds first decides what a full deflection
    /// means, so a generous angular limit cannot raise the side speed past
    /// what the robot may travel.
    #[test]
    fn the_side_speed_takes_whichever_authored_limit_binds_first() {
        // Angular-limited: 2.0 rad/s over a 0.3 m base allows 0.3 m/s a side,
        // well under the 0.6 m/s linear limit.
        let angular_limited = ManualDrive::derive(Some(drive(0.3, 0.6, 2.0)))
            .expect("a differential robot with usable limits");
        assert_eq!(angular_limited.wheel_base_m, 0.3);
        assert_eq!(angular_limited.side_speed_mps, 0.3);

        let linear_limited = ManualDrive::derive(Some(drive(0.3, 0.2, 10.0)))
            .expect("a differential robot with usable limits");
        assert_eq!(linear_limited.side_speed_mps, 0.2);
    }

    /// A robot that cannot be driven manually yields a reason the renderer
    /// matches on, never a sentence composed on another machine.
    #[test]
    fn an_undrivable_robot_yields_a_typed_reason() {
        assert_eq!(
            ManualDrive::derive(None),
            Err(ManualDriveUnsupported::NoDifferentialBase)
        );
        for unusable in ["0.0", "-1.0"] {
            assert_eq!(
                ManualDrive::derive(Some(drive(unusable.parse().unwrap(), 0.6, 2.0))),
                Err(ManualDriveUnsupported::UnusableWheelBase),
                "wheel_base_m: {unusable}"
            );
        }
        assert_eq!(
            ManualDrive::derive(Some(drive(0.3, 0.0, 2.0))),
            Err(ManualDriveUnsupported::MissingMotionLimits)
        );
    }
}
