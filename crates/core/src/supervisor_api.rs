//! Stable, CLI-owned supervisor inspection API.

use phoxal::bus::{ApiVersion, AskQuery, ContractBody, Topic};
use serde::{Deserialize, Serialize};

pub mod v0 {
    use super::*;

    pub const ROBOT_DESCRIPTION_TOPIC: &str = "cli.v0/supervisor/robot-description";

    pub enum Api {}

    impl ApiVersion for Api {
        const ID: &'static str = "cli.v0";
    }

    #[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
    pub struct RobotDescriptionRequest {}

    impl ContractBody for RobotDescriptionRequest {
        type Api = Api;
        const NAME: &'static str = "cli.v0::supervisor::RobotDescriptionRequest";
        const VERSION: &'static str = "cli.v0";
        const CONTRACT: &'static str = "supervisor::RobotDescriptionRequest";
        const TOPIC: &'static str = ROBOT_DESCRIPTION_TOPIC;
    }

    #[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
    pub struct RobotDescription {
        /// The fully parsed and normalized authored robot description.
        pub robot: phoxal::model::robot::RobotV0,
        /// Exact framework train selected by the project Cargo.lock.
        pub framework_train: String,
    }

    impl ContractBody for RobotDescription {
        type Api = Api;
        const NAME: &'static str = "cli.v0::supervisor::RobotDescription";
        const VERSION: &'static str = "cli.v0";
        const CONTRACT: &'static str = "supervisor::RobotDescription";
        const TOPIC: &'static str = ROBOT_DESCRIPTION_TOPIC;
    }

    #[must_use]
    pub fn robot_description_topic() -> Topic<AskQuery<RobotDescriptionRequest, RobotDescription>> {
        Topic::new_static(ROBOT_DESCRIPTION_TOPIC)
    }
}

#[cfg(test)]
mod tests {
    use super::v0::*;
    use phoxal::bus::{Codec, ContractBody, MessagePack};

    #[test]
    fn v0_topic_is_cli_owned_and_versioned() {
        assert_eq!(
            RobotDescription::TOPIC,
            "cli.v0/supervisor/robot-description"
        );
        assert_eq!(RobotDescriptionRequest::TOPIC, RobotDescription::TOPIC);
    }

    #[test]
    fn robot_description_round_trips_with_exact_train_identity() {
        let robot = phoxal::model::robot::RobotV0::parse_from_string(
            r#"schema: robot/v0
robot:
  id: fixture
  namespace: test
  motion_limits:
    max_linear_speed_mps: 0.5
    max_angular_speed_radps: 1.0
  kinematic:
    kind: omnidirectional
    actuators: []
    encoders: []
  components: {}
"#,
        )
        .unwrap();
        let expected = RobotDescription {
            robot,
            framework_train: "0.36.0".into(),
        };
        let encoded = MessagePack::encode(&expected).unwrap();
        let decoded: RobotDescription = MessagePack::decode(&encoded).unwrap();
        assert_eq!(decoded, expected);
    }
}
