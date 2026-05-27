use anyhow::{Result, anyhow};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JointType {
    Fixed,
    Revolute,
    Continuous,
    Prismatic,
}

pub fn has_inertial(link: &urdf_rs::Link) -> bool {
    link.inertial.mass.value > 0.0
}

pub fn convert_joint_type(joint_type: &urdf_rs::JointType) -> Result<JointType> {
    match joint_type {
        urdf_rs::JointType::Fixed => Ok(JointType::Fixed),
        urdf_rs::JointType::Revolute => Ok(JointType::Revolute),
        urdf_rs::JointType::Continuous => Ok(JointType::Continuous),
        urdf_rs::JointType::Prismatic => Ok(JointType::Prismatic),
        urdf_rs::JointType::Floating
        | urdf_rs::JointType::Planar
        | urdf_rs::JointType::Spherical => Err(anyhow!(
            "unsupported URDF joint type {:?} for Webots rendering",
            joint_type
        )),
    }
}

pub fn pose_to_isometry(pose: &urdf_rs::Pose) -> nalgebra::Isometry3<f64> {
    let translation = nalgebra::Translation3::new(pose.xyz[0], pose.xyz[1], pose.xyz[2]);
    let rotation =
        nalgebra::UnitQuaternion::from_euler_angles(pose.rpy[0], pose.rpy[1], pose.rpy[2]);
    nalgebra::Isometry3::from_parts(translation, rotation)
}
