//! Canonical-model-to-Webots pose and joint helpers.

use anyhow::{Result, anyhow};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JointType {
    Fixed,
    Revolute,
    Continuous,
    Prismatic,
}

pub fn has_inertial(link: &phoxal_model::structure::Link) -> bool {
    link.inertial().mass_kg() > 0.0
}

pub fn convert_joint_type(joint_type: phoxal_model::structure::JointKind) -> Result<JointType> {
    use phoxal_model::structure::JointKind;
    match joint_type {
        JointKind::Fixed => Ok(JointType::Fixed),
        JointKind::Revolute => Ok(JointType::Revolute),
        JointKind::Continuous => Ok(JointType::Continuous),
        JointKind::Prismatic => Ok(JointType::Prismatic),
        JointKind::Floating | JointKind::Planar | JointKind::Spherical => Err(anyhow!(
            "unsupported canonical joint type {:?} for Webots rendering",
            joint_type
        )),
    }
}

pub fn pose_to_isometry(pose: phoxal_model::structure::Pose) -> nalgebra::Isometry3<f64> {
    let [x, y, z] = pose.xyz();
    let [roll, pitch, yaw] = pose.rpy();
    let translation = nalgebra::Translation3::new(x, y, z);
    let rotation = nalgebra::UnitQuaternion::from_euler_angles(roll, pitch, yaw);
    nalgebra::Isometry3::from_parts(translation, rotation)
}
