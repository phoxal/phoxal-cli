use phoxal::model::robot::RobotV1 as Robot;
use phoxal_cli::catalog::{CATALOG, SUPPORTED_RUNTIME_TRAIN};
use phoxal_cli::commands::create::robot_yaml;

/// A freshly scaffolded `robot.yaml` must validate out of the box. Robot
/// validation rejects empty actuator/encoder lists, so the scaffold seeds the
/// differential drive lists with placeholder capability references; this test
/// guards against regressing back to empty lists (which broke `validate`/`doctor`
/// on a brand-new project).
#[test]
fn scaffolded_robot_validates_against_catalog() -> anyhow::Result<()> {
    let yaml = robot_yaml("demo-bot", SUPPORTED_RUNTIME_TRAIN);
    let robot = Robot::parse_from_string(&yaml)?;
    robot
        .validate_with(&CATALOG.names().collect::<Vec<_>>())
        .expect("scaffolded robot should validate against the platform catalog");
    Ok(())
}
