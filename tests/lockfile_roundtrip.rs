use phoxal::model::robot::RobotV1 as Robot;
use phoxal_cli::catalog::CATALOG;
use phoxal_cli::lockfile::Lockfile;
use phoxal_cli::releases::ReleasesSnapshot;
use phoxal_cli::resolver::{ResolveOptions, resolve_with_releases};

#[test]
fn lockfile_roundtrips_through_yaml() -> anyhow::Result<()> {
    let robot = Robot::parse_from_string(include_str!("fixtures/plan_robot.yaml"))?;
    let snapshot = ReleasesSnapshot {
        fetched_at: std::time::SystemTime::UNIX_EPOCH,
        versions: vec!["0.7.0".into()],
    };
    let resolved = resolve_with_releases(
        &robot,
        &CATALOG,
        ResolveOptions {
            resolve_external_artifacts: false,
            ..ResolveOptions::default()
        },
        &snapshot,
    )?;
    let lockfile = Lockfile::from_resolved(&resolved);
    assert!(lockfile.phoxal_runtimes.releases_fetched_at.is_some());
    let yaml = serde_yaml::to_string(&lockfile)?;
    let roundtrip: Lockfile = serde_yaml::from_str(&yaml)?;

    assert_eq!(roundtrip, lockfile);
    Ok(())
}
