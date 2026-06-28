use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use clap::{Args, ValueEnum};
use phoxal::model::robot::RobotV1 as Robot;
use toml::Value as TomlValue;

use crate::AppContext;

use crate::catalog::CATALOG;

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
        let platform_names = CATALOG.names_for_api(&robot.api_version);
        if platform_names.is_empty() {
            return Err(anyhow!(
                "API version {} is not available in the compiled-in platform runtime catalog",
                robot.api_version
            ));
        }
        robot
            .validate_with(&platform_names)
            .map_err(|errors| anyhow!("Robot errors:\n{}", join_errors(errors)))?;
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
            platform_names.len()
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

fn check_user_runtime_deps(
    app: &AppContext,
    robot_path: &Path,
    robot: &Robot,
) -> Result<Vec<String>> {
    let robot_root = robot_path
        .parent()
        .context("robot.yaml did not have a parent directory")?;
    let report = collect_user_runtime_dependency_report(robot_root, robot, &robot.api_version);
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
    expected_api_version: &str,
) -> Vec<String> {
    collect_user_runtime_dependency_report(robot_root, robot, expected_api_version).problems
}

fn collect_user_runtime_dependency_report(
    robot_root: &Path,
    robot: &Robot,
    expected_api_version: &str,
) -> UserRuntimeDependencyReport {
    let mut report = UserRuntimeDependencyReport::default();
    for (name, runtime) in &robot.user_runtimes {
        if let Err(error) = crate::resolver::validate_user_runtime_framework_selector(
            name,
            &runtime.framework,
            expected_api_version,
        ) {
            report.problems.push(error.to_string());
        }
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
                "user runtime '{name}' is missing a phoxal dependency; add a phoxal dependency built for API {expected_api_version}"
            ));
            continue;
        };

        match phoxal_dependency(dep) {
            PhoxalDependency::Branch(branch) => report.problems.push(format!(
                "user runtime '{name}' floats on branch '{branch}'; pin it to a phoxal release or tag built for API {expected_api_version}"
            )),
            PhoxalDependency::Pinned(version) => report.successes.push(format!(
                "user runtime '{name}' declares phoxal dependency {version}; expected graph API {expected_api_version}"
            )),
            PhoxalDependency::Unparsable => report.problems.push(format!(
                "user runtime '{name}' has an unparsable phoxal dependency; pin it to a phoxal release or tag built for API {expected_api_version}"
            )),
        }
    }

    report
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

fn print_text_report(robot: &Robot) {
    println!("robot: {}", robot.identity.id);
    println!("api_version: {}", robot.api_version);
    println!("channel: {}", robot.phoxal_runtimes.channel);
    println!("platform_runtimes:");
    for runtime in CATALOG.entries_for_api(&robot.api_version) {
        let image_ref = robot
            .phoxal_runtimes
            .images
            .get(runtime.name)
            .cloned()
            .unwrap_or_else(|| {
                format!(
                    "{}:{}-{}",
                    runtime.image_repo(),
                    robot.api_version,
                    robot.phoxal_runtimes.channel
                )
            });
        println!("  - {} -> {}", runtime.name, image_ref);
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
        "api_version": robot.api_version,
        "channel": robot.phoxal_runtimes.channel,
        "platform_runtimes": CATALOG.entries_for_api(&robot.api_version).map(|runtime| {
            let image_ref = robot
                .phoxal_runtimes
                .images
                .get(runtime.name)
                .cloned()
                .unwrap_or_else(|| {
                    format!(
                        "{}:{}-{}",
                        runtime.image_repo(),
                        robot.api_version,
                        robot.phoxal_runtimes.channel
                    )
                });
            serde_json::json!({
                "name": runtime.name,
                "api_versions": runtime.api_versions,
                "image_ref": image_ref,
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
    fn pinned_user_runtime_phoxal_dep_collects_no_problems() -> anyhow::Result<()> {
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
phoxal = "0.14.0"
"#,
        )?;
        let robot = robot_with_user_runtime("runtimes/drive")?;

        let problems = collect_user_runtime_problems(temp.path(), &robot, "y2026_1");

        assert!(problems.is_empty(), "unexpected problems: {problems:?}");
        Ok(())
    }

    #[test]
    fn branch_user_runtime_phoxal_dep_collects_problem() -> anyhow::Result<()> {
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
phoxal = { git = "https://github.com/phoxal/framework", branch = "main" }
"#,
        )?;
        let robot = robot_with_user_runtime("runtimes/drive")?;

        let problems = collect_user_runtime_problems(temp.path(), &robot, "y2026_1");

        assert_eq!(
            problems,
            vec![
                "user runtime 'drive' floats on branch 'main'; pin it to a phoxal release or tag built for API y2026_1"
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

        let problems = collect_user_runtime_problems(temp.path(), &robot, "y2026_1");

        assert_eq!(
            problems,
            vec![
                "user runtime 'drive' is missing a phoxal dependency; add a phoxal dependency built for API y2026_1"
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

        let problems = collect_user_runtime_problems(temp.path(), &robot, "y2026_1");

        assert_eq!(
            problems,
            vec![format!(
                "user runtime 'drive' has no readable Cargo.toml at {}; cannot check phoxal dependency",
                runtime_dir.join("Cargo.toml").display()
            )]
        );
        Ok(())
    }

    #[test]
    fn empty_user_runtime_framework_collects_problem() -> anyhow::Result<()> {
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
phoxal = "0.14.0"
"#,
        )?;
        let robot = Robot::parse_from_string(
            r#"schema: v0
api_version: y2026_1

identity:
  id: testbot
  namespace: test

structure: structure.urdf

phoxal_runtimes:
  channel: stable

user_runtimes:
  drive:
    path: runtimes/drive
    framework: ""

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
  sources: {}
  instances: {}
"#,
        )?;

        let problems = collect_user_runtime_problems(temp.path(), &robot, "y2026_1");

        assert_eq!(
            problems,
            vec![
                "user runtime 'drive': framework '' must be \"match-platform\" or the graph api_version 'y2026_1'"
                    .to_string()
            ]
        );
        Ok(())
    }

    fn write_manifest(runtime_dir: &Path, manifest: &str) -> anyhow::Result<()> {
        fs::create_dir_all(runtime_dir)?;
        fs::write(runtime_dir.join("Cargo.toml"), manifest)?;
        Ok(())
    }

    fn robot_with_user_runtime(runtime_path: &str) -> anyhow::Result<Robot> {
        Robot::parse_from_string(&format!(
            r#"schema: v0
api_version: y2026_1

identity:
  id: testbot
  namespace: test

structure: structure.urdf

phoxal_runtimes:
  channel: stable

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
    }
}
