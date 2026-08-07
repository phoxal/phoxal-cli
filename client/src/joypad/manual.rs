//! Turning a robot's authored model into manual-drive parameters.
//!
//! This used to travel on the supervisor snapshot: the daemon derived it and
//! republished a derived copy on every revision. It does not any more.
//! The pad is attached to the machine running this client,
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
        let limits = &robot.motion_limits;
        if !(limits.max_linear_speed_mps.is_finite()
            && limits.max_linear_speed_mps > 0.0
            && limits.max_angular_speed_radps.is_finite()
            && limits.max_angular_speed_radps > 0.0)
        {
            return Err(ManualDriveUnsupported::MissingMotionLimits);
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A finalized `robot.yaml`, exactly as the daemon serves it through
    /// `bundle/get` - which is where these parameters come from now
    ///.
    fn manifest(kinematic: &str, motion: &str) -> phoxal_manifest::source::robot::v0::Manifest {
        let text = format!(
            "schema: phoxal/robot/v0\n\
             clock: real\n\
             robot:\n\
             \x20 id: rover\n\
             \x20 namespace: lab\n\
             \x20 kinematic:\n{kinematic}\
             \x20 motion_limits:\n{motion}"
        );
        let phoxal_manifest::source::robot::Manifest::V0(body) =
            phoxal_manifest::source::robot::parse_from_string(&text).unwrap_or_else(|error| {
                panic!("the fixture manifest must parse: {error:#}\n{text}")
            });
        body
    }

    const DIFFERENTIAL: &str = "\x20   kind: differential\n\
                                \x20   left_actuators: [base.left]\n\
                                \x20   right_actuators: [base.right]\n\
                                \x20   left_encoders: [base.left_enc]\n\
                                \x20   right_encoders: [base.right_enc]\n\
                                \x20   wheel_radius_m: 0.05\n\
                                \x20   wheel_base_m: 0.3\n";

    const OMNIDIRECTIONAL: &str = "\x20   kind: omnidirectional\n\
                                   \x20   actuators: [base.a]\n\
                                   \x20   encoders: [base.e]\n";

    fn limits(linear: &str, angular: &str) -> String {
        format!(
            "\x20   max_linear_speed_mps: {linear}\n\x20   max_angular_speed_radps: {angular}\n"
        )
    }

    /// Whichever authored limit binds first decides what a full deflection
    /// means, so a generous angular limit cannot raise the side speed past
    /// what the robot may travel.
    #[test]
    fn the_side_speed_takes_whichever_authored_limit_binds_first() {
        // Angular-limited: 2.0 rad/s over a 0.3 m base allows 0.3 m/s a side,
        // well under the 0.6 m/s linear limit.
        let angular_limited =
            ManualDrive::derive(&manifest(DIFFERENTIAL, &limits("0.6", "2.0")).robot)
                .expect("a differential robot with usable limits");
        assert_eq!(angular_limited.wheel_base_m, 0.3);
        assert_eq!(angular_limited.side_speed_mps, 0.3);

        let linear_limited =
            ManualDrive::derive(&manifest(DIFFERENTIAL, &limits("0.2", "10.0")).robot)
                .expect("a differential robot with usable limits");
        assert_eq!(linear_limited.side_speed_mps, 0.2);
    }

    /// A robot that cannot be driven manually yields a reason the renderer
    /// matches on, never a sentence composed on another machine.
    #[test]
    fn an_undrivable_robot_yields_a_typed_reason() {
        assert_eq!(
            ManualDrive::derive(&manifest(OMNIDIRECTIONAL, &limits("0.6", "2.0")).robot),
            Err(ManualDriveUnsupported::NoDifferentialBase)
        );
        for unusable in ["0.0", "-1.0"] {
            let kinematic =
                DIFFERENTIAL.replace("wheel_base_m: 0.3", &format!("wheel_base_m: {unusable}"));
            assert_eq!(
                ManualDrive::derive(&manifest(&kinematic, &limits("0.6", "2.0")).robot),
                Err(ManualDriveUnsupported::UnusableWheelBase),
                "wheel_base_m: {unusable}"
            );
        }
        assert_eq!(
            ManualDrive::derive(&manifest(DIFFERENTIAL, &limits("0.0", "2.0")).robot),
            Err(ManualDriveUnsupported::MissingMotionLimits)
        );
    }
}
