use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use clap::{Args, ValueEnum};
use phoxal::model::robot::RobotV1 as Robot;
use semver::{Version, VersionReq};
use toml::Value as TomlValue;

use crate::AppContext;

use crate::catalog::{CATALOG, SUPPORTED_RUNTIME_TRAIN};
use crate::lockfile::{LOCKFILE_NAME, Lockfile};

#[derive(Debug, Args)]
pub struct Validate {
    #[arg(long, help = "Print the derived runtime/component graph.")]
    pub report: bool,
    #[arg(long, value_enum, default_value_t = ReportFormat::Text)]
    pub report_format: ReportFormat,
    #[arg(
        long,
        help = "Downgrade user-runtime framework mismatches from errors to warnings (local dev only)."
    )]
    pub allow_user_runtime_drift: bool,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum ReportFormat {
    Text,
    Json,
}

impl Validate {
    pub async fn run(&self, app: &AppContext) -> Result<()> {
        let robot_path = crate::resolver::discover_robot_yaml(app.project.root())?;
        let robot = crate::resolver::load_robot(&robot_path)?;
        let platform_names = CATALOG.names_vec();
        robot
            .validate_with(&platform_names)
            .map_err(|errors| anyhow!("Robot errors:\n{}", join_errors(errors)))?;
        validate_runtime_selector(&robot.phoxal_runtimes.version)?;
        let user_runtime_problems = check_user_runtime_deps(app, &robot_path, &robot)?;
        if !user_runtime_problems.is_empty() {
            if self.allow_user_runtime_drift {
                for problem in user_runtime_problems {
                    app.ui.warn(problem);
                }
            } else {
                return Err(anyhow!(
                    "User runtime framework dependency check failed:\n{}\n\nFix these user runtime Cargo.toml files or rerun with --allow-user-runtime-drift for local dev only.",
                    user_runtime_problems.join("\n")
                ));
            }
        }
        app.ui.success(format!(
            "validated {} with {} platform runtimes",
            robot_path.display(),
            CATALOG.entries.len()
        ));
        if self.report {
            match self.report_format {
                ReportFormat::Text => print_text_report(&robot),
                ReportFormat::Json => print_json_report(&robot)?,
            }
        }
        Ok(())
    }
}

fn validate_runtime_selector(selector: &str) -> Result<()> {
    if selector == "latest" || Version::parse(selector).is_ok() {
        return Ok(());
    }
    VersionReq::parse(selector)
        .map(|_| ())
        .with_context(|| format!("invalid phoxal_runtimes.version selector '{selector}'"))
}

fn check_user_runtime_deps(
    app: &AppContext,
    robot_path: &Path,
    robot: &Robot,
) -> Result<Vec<String>> {
    let robot_root = robot_path
        .parent()
        .context("robot.yaml did not have a parent directory")?;
    let expected = expected_runtime_train(app, robot_root);
    let Some(expected_train) = parse_runtime_train(&expected) else {
        app.ui.warn(format!(
            "could not parse supported platform runtime train {expected}; skipping user runtime dependency check"
        ));
        return Ok(Vec::new());
    };

    let report =
        collect_user_runtime_dependency_report(robot_root, robot, &expected, expected_train);
    for success in report.successes {
        app.ui.success(success);
    }
    Ok(report.problems)
}

#[derive(Debug, Default, PartialEq, Eq)]
struct UserRuntimeDependencyReport {
    problems: Vec<String>,
    successes: Vec<String>,
}

#[cfg(test)]
fn collect_user_runtime_problems(
    robot_root: &Path,
    robot: &Robot,
    expected: &str,
    expected_train: RuntimeTrain,
) -> Vec<String> {
    collect_user_runtime_dependency_report(robot_root, robot, expected, expected_train).problems
}

fn collect_user_runtime_dependency_report(
    robot_root: &Path,
    robot: &Robot,
    expected: &str,
    expected_train: RuntimeTrain,
) -> UserRuntimeDependencyReport {
    let mut report = UserRuntimeDependencyReport::default();
    for (name, runtime) in &robot.user_runtimes {
        let runtime_dir = resolve_robot_path(robot_root, &runtime.path);
        let manifest_path = runtime_dir.join("Cargo.toml");
        let Ok(contents) = fs::read_to_string(&manifest_path) else {
            report.problems.push(format!(
                "user runtime '{name}' has no readable Cargo.toml at {}; cannot check phoxal dependency",
                manifest_path.display()
            ));
            continue;
        };
        let Ok(manifest) = toml::from_str::<TomlValue>(&contents) else {
            report.problems.push(format!(
                "user runtime '{name}' has an unparsable Cargo.toml at {}; cannot check phoxal dependency",
                manifest_path.display()
            ));
            continue;
        };
        let Some(dep) = manifest
            .get("dependencies")
            .and_then(|dependencies| dependencies.get("phoxal"))
        else {
            report.problems.push(format!(
                "user runtime '{name}' is missing a phoxal dependency; pin it to the platform runtime train {expected}"
            ));
            continue;
        };

        match phoxal_dependency(dep) {
            PhoxalDependency::Branch(branch) => report.problems.push(format!(
                "user runtime '{name}' floats on branch '{branch}'; pin it to the platform runtime train {expected}"
            )),
            PhoxalDependency::Pinned(version) => match parse_runtime_train(&version) {
                Some(actual_train) if actual_train == expected_train => report.successes.push(format!(
                    "user runtime '{name}' phoxal dependency matches platform runtime train {expected}"
                )),
                Some(_) => report.problems.push(format!(
                    "user runtime '{name}' pins phoxal {version}; expected platform runtime train {expected}"
                )),
                None => report.problems.push(format!(
                    "user runtime '{name}' has an unparsable phoxal dependency {version}; pin it to the platform runtime train {expected}"
                )),
            },
            PhoxalDependency::Unparsable => report.problems.push(format!(
                "user runtime '{name}' has an unparsable phoxal dependency; pin it to the platform runtime train {expected}"
            )),
        }
    }

    report
}

fn expected_runtime_train(app: &AppContext, robot_root: &Path) -> String {
    let lock_path = robot_root.join(LOCKFILE_NAME);
    if !lock_path.is_file() {
        return SUPPORTED_RUNTIME_TRAIN.to_string();
    }
    match Lockfile::read(&lock_path) {
        Ok(lockfile) => lockfile.phoxal_runtimes.resolved,
        Err(err) => {
            app.ui.warn(format!(
                "failed to read {}; checking user runtime deps against {SUPPORTED_RUNTIME_TRAIN}: {err:#}",
                lock_path.display()
            ));
            SUPPORTED_RUNTIME_TRAIN.to_string()
        }
    }
}

fn resolve_robot_path(robot_root: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        robot_root.join(path)
    }
}

enum PhoxalDependency {
    Branch(String),
    Pinned(String),
    Unparsable,
}

fn phoxal_dependency(dep: &TomlValue) -> PhoxalDependency {
    if let Some(version) = dep.as_str() {
        return PhoxalDependency::Pinned(version.to_string());
    }
    let Some(table) = dep.as_table() else {
        return PhoxalDependency::Unparsable;
    };
    if let Some(branch) = table.get("branch").and_then(TomlValue::as_str) {
        return PhoxalDependency::Branch(branch.to_string());
    }
    if let Some(version) = table.get("version").and_then(TomlValue::as_str) {
        return PhoxalDependency::Pinned(version.to_string());
    }
    if let Some(tag) = table.get("tag").and_then(TomlValue::as_str) {
        return PhoxalDependency::Pinned(tag.trim_start_matches('v').to_string());
    }
    PhoxalDependency::Unparsable
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RuntimeTrain {
    major: u64,
    minor: u64,
}

fn parse_runtime_train(value: &str) -> Option<RuntimeTrain> {
    let trimmed = value.trim().trim_start_matches('v');
    for token in trimmed
        .split(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '.' || ch == '-' || ch == '+'))
    {
        let token = token.trim_start_matches('v');
        let mut parts = token.split('.');
        let (Some(major), Some(minor)) = (parts.next(), parts.next()) else {
            continue;
        };
        if !major.chars().all(|ch| ch.is_ascii_digit())
            || !minor.chars().all(|ch| ch.is_ascii_digit())
        {
            continue;
        }
        return Some(RuntimeTrain {
            major: major.parse().ok()?,
            minor: minor.parse().ok()?,
        });
    }
    None
}

fn print_text_report(robot: &Robot) {
    println!("robot: {}", robot.identity.id);
    println!("runtime_set: {}", robot.phoxal_runtimes.version);
    println!("platform_runtimes:");
    for runtime in CATALOG.entries {
        let version = robot
            .phoxal_runtimes
            .overrides
            .get(runtime.name)
            .and_then(|runtime| runtime.version.as_deref())
            .unwrap_or(&robot.phoxal_runtimes.version);
        println!("  - {} -> {}:{}", runtime.name, runtime.image_repo, version);
    }
    println!("user_runtimes:");
    for (name, runtime) in &robot.user_runtimes {
        println!("  - {} -> {}", name, runtime.path.display());
    }
    println!("components:");
    for (instance_name, instance) in &robot.components.instances {
        let driver = if instance.driver.is_some() {
            "driver"
        } else {
            "no-driver"
        };
        println!(
            "  - {} ({}) from {}",
            instance_name, driver, instance.component
        );
    }
}

fn print_json_report(robot: &Robot) -> Result<()> {
    let report = serde_json::json!({
        "robot": robot.identity.id,
        "runtime_set": robot.phoxal_runtimes.version,
        "platform_runtimes": CATALOG.entries.iter().map(|runtime| {
            let version = robot
                .phoxal_runtimes
                .overrides
                .get(runtime.name)
                .and_then(|runtime| runtime.version.as_deref())
                .unwrap_or(&robot.phoxal_runtimes.version);
            serde_json::json!({
                "name": runtime.name,
                "image_repo": runtime.image_repo,
                "version": version,
            })
        }).collect::<Vec<_>>(),
        "user_runtimes": robot.user_runtimes.iter().map(|(name, runtime)| {
            serde_json::json!({
                "name": name,
                "path": runtime.path,
            })
        }).collect::<Vec<_>>(),
        "components": robot.components.instances.iter().map(|(instance_name, instance)| {
            serde_json::json!({
                "instance": instance_name,
                "source": instance.component,
                "has_driver": instance.driver.is_some(),
            })
        }).collect::<Vec<_>>(),
    });
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

fn join_errors(errors: Vec<phoxal::model::robot::ValidationError>) -> String {
    errors
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXPECTED_RUNTIME_TRAIN: &str = "^0.8";

    #[test]
    fn parses_version_and_requirement_trains() {
        let train = RuntimeTrain { major: 0, minor: 7 };

        assert_eq!(parse_runtime_train("0.7.0"), Some(train));
        assert_eq!(parse_runtime_train("v0.7.1"), Some(train));
        assert_eq!(parse_runtime_train("^0.7"), Some(train));
        assert_eq!(parse_runtime_train(">=0.7, <0.8"), Some(train));
        assert_eq!(parse_runtime_train("main"), None);
    }

    #[test]
    fn classifies_phoxal_dependency_forms() {
        let manifest: TomlValue = toml::from_str(
            r#"
[dependencies]
string = "0.7.0"
table = { version = "^0.7" }
tag = { git = "https://github.com/phoxal/framework", tag = "v0.7.0" }
branch = { git = "https://github.com/phoxal/framework", branch = "main" }
"#,
        )
        .expect("valid toml");
        let dependencies = manifest
            .get("dependencies")
            .expect("dependencies")
            .as_table()
            .expect("dependencies table");

        assert!(matches!(
            phoxal_dependency(dependencies.get("string").expect("string")),
            PhoxalDependency::Pinned(version) if version == "0.7.0"
        ));
        assert!(matches!(
            phoxal_dependency(dependencies.get("table").expect("table")),
            PhoxalDependency::Pinned(version) if version == "^0.7"
        ));
        assert!(matches!(
            phoxal_dependency(dependencies.get("tag").expect("tag")),
            PhoxalDependency::Pinned(version) if version == "0.7.0"
        ));
        assert!(matches!(
            phoxal_dependency(dependencies.get("branch").expect("branch")),
            PhoxalDependency::Branch(branch) if branch == "main"
        ));
    }

    #[test]
    fn matching_user_runtime_train_collects_no_problems() -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        let runtime_dir = temp.path().join("runtimes/drive");
        write_manifest(
            &runtime_dir,
            r#"
[package]
name = "drive"
version = "0.1.0"
edition = "2024"

[dependencies]
phoxal = "0.8.5"
"#,
        )?;
        let robot = robot_with_user_runtime("runtimes/drive")?;

        let problems = collect_user_runtime_problems(
            temp.path(),
            &robot,
            EXPECTED_RUNTIME_TRAIN,
            parse_expected_runtime_train(),
        );

        assert!(problems.is_empty(), "unexpected problems: {problems:?}");
        Ok(())
    }

    #[test]
    fn mismatched_user_runtime_pin_collects_problem() -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        let runtime_dir = temp.path().join("runtimes/drive");
        write_manifest(
            &runtime_dir,
            r#"
[package]
name = "drive"
version = "0.1.0"
edition = "2024"

[dependencies]
phoxal = "0.7.0"
"#,
        )?;
        let robot = robot_with_user_runtime("runtimes/drive")?;

        let problems = collect_user_runtime_problems(
            temp.path(),
            &robot,
            EXPECTED_RUNTIME_TRAIN,
            parse_expected_runtime_train(),
        );

        assert_eq!(
            problems,
            vec![
                "user runtime 'drive' pins phoxal 0.7.0; expected platform runtime train ^0.8"
                    .to_string()
            ]
        );
        Ok(())
    }

    #[test]
    fn missing_user_runtime_phoxal_dep_collects_problem() -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        let runtime_dir = temp.path().join("runtimes/drive");
        write_manifest(
            &runtime_dir,
            r#"
[package]
name = "drive"
version = "0.1.0"
edition = "2024"

[dependencies]
serde = "1"
"#,
        )?;
        let robot = robot_with_user_runtime("runtimes/drive")?;

        let problems = collect_user_runtime_problems(
            temp.path(),
            &robot,
            EXPECTED_RUNTIME_TRAIN,
            parse_expected_runtime_train(),
        );

        assert_eq!(
            problems,
            vec![
                "user runtime 'drive' is missing a phoxal dependency; pin it to the platform runtime train ^0.8"
                    .to_string()
            ]
        );
        Ok(())
    }

    #[test]
    fn missing_user_runtime_manifest_collects_problem() -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        let runtime_dir = temp.path().join("runtimes/drive");
        let robot = robot_with_user_runtime("runtimes/drive")?;

        let problems = collect_user_runtime_problems(
            temp.path(),
            &robot,
            EXPECTED_RUNTIME_TRAIN,
            parse_expected_runtime_train(),
        );

        assert_eq!(
            problems,
            vec![format!(
                "user runtime 'drive' has no readable Cargo.toml at {}; cannot check phoxal dependency",
                runtime_dir.join("Cargo.toml").display()
            )]
        );
        Ok(())
    }

    fn parse_expected_runtime_train() -> RuntimeTrain {
        parse_runtime_train(EXPECTED_RUNTIME_TRAIN).expect("expected runtime train parses")
    }

    fn write_manifest(runtime_dir: &Path, manifest: &str) -> anyhow::Result<()> {
        fs::create_dir_all(runtime_dir)?;
        fs::write(runtime_dir.join("Cargo.toml"), manifest)?;
        Ok(())
    }

    fn robot_with_user_runtime(runtime_path: &str) -> anyhow::Result<Robot> {
        Robot::parse_from_string(&format!(
            r#"version: v1

identity:
  id: testbot
  namespace: test

structure: structure.urdf

phoxal_runtimes:
  version: "latest"

user_runtimes:
  drive:
    path: {runtime_path}

motion:
  kinematic:
    kind: differential
    left_actuators: [left_drive.motor]
    right_actuators: [right_drive.motor]
    left_encoders: [left_drive.encoder]
    right_encoders: [right_drive.encoder]
    wheel_radius_m: 0.1
    wheel_base_m: 0.5

components:
  sources:
    ddsm115:
      path: ./components/ddsm115
  instances:
    left_drive:
      component: ddsm115
      mount_link: left_wheel_mount
    right_drive:
      component: ddsm115
      mount_link: right_wheel_mount
"#
        ))
        .map_err(Into::into)
    }
}
