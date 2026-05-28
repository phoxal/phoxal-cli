use phoxal_cli::catalog::CATALOG;
use phoxal_robot::RobotV1 as Robot;

#[test]
fn plan_robot_validates_against_catalog() -> anyhow::Result<()> {
    let robot = Robot::parse_from_string(include_str!("fixtures/plan_robot.yaml"))?;
    robot
        .validate_with(&CATALOG.names().collect::<Vec<_>>())
        .expect("plan robot should validate against the platform catalog");
    assert_eq!(CATALOG.names().count(), CATALOG.entries.len());
    Ok(())
}
