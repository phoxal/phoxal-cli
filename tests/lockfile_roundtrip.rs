use phoxal_cli::catalog::CATALOG;
use phoxal_cli::lockfile::Lockfile;
use phoxal_cli::resolver::{ResolveOptions, resolve};
use phoxal_utils_robot::Robot;

#[test]
fn lockfile_roundtrips_through_yaml() -> anyhow::Result<()> {
    let robot = Robot::parse_from_string(include_str!(
        "../../engine/utils-robot/tests/fixtures/plan_robot.yaml"
    ))?;
    let resolved = resolve(
        &robot,
        &CATALOG,
        ResolveOptions {
            resolve_external_artifacts: false,
            ..ResolveOptions::default()
        },
    )?;
    let lockfile = Lockfile::from_resolved(&resolved);
    let yaml = serde_yaml::to_string(&lockfile)?;
    let roundtrip: Lockfile = serde_yaml::from_str(&yaml)?;

    assert_eq!(roundtrip, lockfile);
    Ok(())
}
