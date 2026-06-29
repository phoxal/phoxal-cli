use std::fs;

use phoxal::model::robot::RobotV1 as Robot;
use phoxal_cli::catalog::CATALOG;
use phoxal_cli::lockfile::Lockfile;
use phoxal_cli::resolver::{ResolveOptions, resolve};

#[test]
fn lockfile_roundtrips_through_yaml() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    write_runtime_source(temp.path(), "runtimes/mission_behavior")?;
    write_runtime_source(temp.path(), "runtimes/inspection_policy")?;
    let robot = Robot::parse_from_string(include_str!("fixtures/plan_robot.yaml"))?;
    let resolved = resolve(
        &robot,
        temp.path(),
        &CATALOG,
        // Fully offline: no `git ls-remote` for component commits, no registry
        // digest/tool-hash network. The lock still round-trips with the empty
        // commits the offline resolve leaves behind.
        ResolveOptions {
            locked: false,
            resolve_external_artifacts: false,
            resolve_source_commits: false,
        },
    )?;
    let lockfile = Lockfile::from_resolved(&resolved);
    assert_eq!(lockfile.phoxal_runtimes.api_version, "y2026_1");
    assert_eq!(lockfile.phoxal_runtimes.channel, "stable");
    let yaml = serde_yaml::to_string(&lockfile)?;
    let roundtrip: Lockfile = serde_yaml::from_str(&yaml)?;

    assert_eq!(roundtrip, lockfile);
    Ok(())
}

fn write_runtime_source(root: &std::path::Path, path: &str) -> anyhow::Result<()> {
    let dir = root.join(path);
    fs::create_dir_all(&dir)?;
    fs::write(dir.join("Dockerfile"), "FROM scratch\n")?;
    Ok(())
}
