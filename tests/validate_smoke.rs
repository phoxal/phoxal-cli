use phoxal::model::robot::RobotV0 as Robot;

#[test]
fn plan_robot_validates_against_catalog() -> anyhow::Result<()> {
    let robot = Robot::parse_from_string(include_str!("fixtures/plan_robot.yaml"))?;
    robot
        .validate_with(&service_names())
        .expect("plan robot should validate against the platform catalog");
    assert_eq!(service_names().len(), 13);
    Ok(())
}

fn service_names() -> Vec<&'static str> {
    vec![
        "asset",
        "drive",
        "frame",
        "joint",
        "localize",
        "map",
        "motion",
        "navigation",
        "odometry",
        "perception",
        "power",
        "presence",
        "video",
    ]
}
