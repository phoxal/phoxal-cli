use phoxal::model::robot::RobotV1 as Robot;
use phoxal_cli::catalog::CATALOG;

#[test]
fn plan_robot_validates_against_catalog() -> anyhow::Result<()> {
    let robot = Robot::parse_from_string(include_str!("fixtures/plan_robot.yaml"))?;
    robot
        .validate_with(&CATALOG.names_for_api(&robot.api_version))
        .expect("plan robot should validate against the platform catalog");
    assert_eq!(CATALOG.names_for_api(&robot.api_version).len(), 6);
    Ok(())
}
