use phoxal::model::robot::RobotV1 as Robot;

#[test]
fn plan_robot_validates_against_catalog() -> anyhow::Result<()> {
    let robot = Robot::parse_from_string(include_str!("fixtures/plan_robot.yaml"))?;
    robot
        .validate_with(&service_names())
        .expect("plan robot should validate against the platform catalog");
    assert_eq!(service_names().len(), 18);
    Ok(())
}

fn service_names() -> Vec<&'static str> {
    vec![
        "asset",
        "battery",
        "drive",
        "explore",
        "follow",
        "frame",
        "joint",
        "localize",
        "map",
        "mission",
        "motion",
        "odometry",
        "perception",
        "plan",
        "power",
        "presence",
        "safety",
        "video",
    ]
}
