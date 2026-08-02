//! The identity of one robot on an attachment.

/// Which robot an observation belongs to.
///
/// This is identity, not a feed. It used to live beside the bus observations
/// and outlived them (organization#978).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct RobotScope {
    pub namespace: String,
    pub robot_id: String,
}
