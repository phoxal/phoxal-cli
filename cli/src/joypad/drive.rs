//! Turning shoulder inputs into a normalized `ManualCommand`.
//!
//! The mixing is pure and hardware-free so it can be unit-tested without a
//! gamepad: [`super::Joypad`] reads the four trigger values from `gilrs` and
//! hands them here.

use phoxal_client::robot as api;

/// Below this, a resting trigger reads as zero rather than as creep.
const TRIGGER_DEADZONE: f32 = 0.08;

/// Differential shoulder preset: L2/R2 are the left/right forward triggers;
/// L1/R1 reverse only their matching trigger. A modifier alone is zero.
///
/// Both sides are already normalized deflections in `-1.0..=1.0`, and the twist
/// is the geometry-free tank mix of them: the mean is forward intent and the
/// half-difference is turn intent. No wheel base and no speed limit appears
/// here, because the robot scales this intent by its own authored limits -
/// physics stays where the kinematics already live.
pub(super) fn command_from_shoulders(
    reverse_left: f32,
    forward_left: f32,
    reverse_right: f32,
    forward_right: f32,
) -> api::motion::ManualCommand {
    let left = side_input(reverse_left, forward_left);
    let right = side_input(reverse_right, forward_right);
    api::motion::ManualCommand {
        linear: (left + right) / 2.0,
        angular: (right - left) / 2.0,
    }
}

fn side_input(reverse_modifier: f32, forward_trigger: f32) -> f64 {
    let magnitude = f64::from(trigger(forward_trigger));
    if reverse_modifier >= 0.5 {
        -magnitude
    } else {
        magnitude
    }
}

fn trigger(value: f32) -> f32 {
    if !value.is_finite() || value.abs() < TRIGGER_DEADZONE {
        0.0
    } else {
        value.clamp(0.0, 1.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_triggers_forward_drive_straight() {
        let command = command_from_shoulders(0.0, 1.0, 0.0, 1.0);
        assert_eq!(command.linear, 1.0);
        assert_eq!(command.angular, 0.0);
    }

    #[test]
    fn one_side_forward_turns_toward_the_idle_side() {
        let command = command_from_shoulders(0.0, 0.0, 0.0, 1.0);
        assert_eq!(command.linear, 0.5);
        // Right side alone turns counter-clockwise: +right / 2.
        assert_eq!(command.angular, 0.5);
    }

    #[test]
    fn a_modifier_reverses_only_its_own_side() {
        let command = command_from_shoulders(1.0, 1.0, 0.0, 1.0);
        assert_eq!(command.linear, 0.0);
        // A full spin in place is full turn deflection and no forward intent.
        assert_eq!(command.angular, 1.0);
    }

    #[test]
    fn a_modifier_alone_is_not_motion() {
        let command = command_from_shoulders(1.0, 0.0, 1.0, 0.0);
        assert_eq!(command.linear, 0.0);
        assert_eq!(command.angular, 0.0);
    }

    #[test]
    fn a_resting_or_nonfinite_trigger_reads_as_zero() {
        // A resting trigger that never quite returns to 0.0 must not creep,
        // and a backend reporting NaN must not reach the arithmetic.
        for resting in [0.0, TRIGGER_DEADZONE - 0.001, f32::NAN] {
            let command = command_from_shoulders(0.0, resting, 0.0, resting);
            assert_eq!(command.linear, 0.0, "resting value {resting}");
            assert_eq!(command.angular, 0.0, "resting value {resting}");
        }
    }

    /// A magnitude past 1 is full deflection, so an over-range trigger must be
    /// clamped here rather than sent as an out-of-contract fraction.
    #[test]
    fn an_out_of_range_trigger_is_clamped_to_full_scale() {
        let command = command_from_shoulders(0.0, 4.0, 0.0, 4.0);
        assert_eq!(command.linear, 1.0);
        assert_eq!(command.angular, 0.0);
    }
}
