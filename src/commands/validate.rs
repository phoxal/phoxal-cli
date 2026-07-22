use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use clap::{Args, ValueEnum};
use phoxal::model::robot::RobotV0 as Robot;
use toml::Value as TomlValue;

use crate::AppContext;

#[derive(Debug, Args)]
pub struct Validate {
    #[arg(long, help = "Print the derived service/component graph.")]
    pub report: bool,
    #[arg(
        long,
        value_enum,
        default_value_t = ReportFormat::Text,
        help = "Format for the --report output."
    )]
    pub report_format: ReportFormat,
    #[arg(
        long,
        help = "Downgrade user-service framework mismatches from errors to warnings (local dev only)."
    )]
    pub allow_user_service_drift: bool,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum ReportFormat {
    Text,
    Json,
}

impl Validate {
    pub async fn run(&self, app: &AppContext) -> Result<()> {
        let robot_path =
            phoxal_cli_core::project::resolver::discover_robot_yaml(app.project.root())?;
        let project_root = robot_path
            .parent()
            .context("robot.yaml did not have a parent directory")?;
        let loaded = phoxal_cli_core::project::resolver::load_robot_with_extras(&robot_path)?;
        let robot = loaded.robot;
        let suite = crate::commands::load_suite_for_robot(app, project_root, &loaded.extras)?;
        let platform_names = suite.as_ref().map_or_else(Vec::new, |suite| {
            phoxal_cli_core::project::suite::artifacts_of_kind(
                suite,
                phoxal_cli_core::project::suite::Kind::Service,
            )
            .into_iter()
            .map(|artifact| {
                artifact
                    .id
                    .trim_start_matches("phoxal/service-")
                    .to_string()
            })
            .collect()
        });
        let platform_name_refs = platform_names
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>();
        robot
            .validate_with(&platform_name_refs)
            .map_err(|errors| anyhow!("Robot errors:\n{}", join_errors(errors)))?;
        let user_service_problems = check_user_service_deps(app, &robot_path, &robot)?;
        if !user_service_problems.is_empty() {
            if self.allow_user_service_drift {
                for problem in user_service_problems {
                    app.ui.warn(problem);
                }
            } else {
                return Err(anyhow!(
                    "User service framework dependency check failed:\n{}\n\nFix these user service Cargo.toml files or rerun with --allow-user-service-drift for local dev only.",
                    user_service_problems.join("\n")
                ));
            }
        }
        app.ui.success(format!(
            "validated {} with {} official services",
            robot_path.display(),
            platform_names.len()
        ));
        if self.report {
            match self.report_format {
                ReportFormat::Text => print_text_report(&robot, suite.as_ref()),
                ReportFormat::Json => print_json_report(&robot, suite.as_ref())?,
            }
        }
        Ok(())
    }
}

fn check_user_service_deps(
    app: &AppContext,
    robot_path: &Path,
    robot: &Robot,
) -> Result<Vec<String>> {
    let robot_root = robot_path
        .parent()
        .context("robot.yaml did not have a parent directory")?;
    let report = collect_user_service_dependency_report(robot_root, robot);
    for success in report.successes {
        app.ui.success(success);
    }
    Ok(report.problems)
}

#[derive(Debug, Default, PartialEq, Eq)]
struct UserServiceDependencyReport {
    problems: Vec<String>,
    successes: Vec<String>,
}

#[cfg(test)]
fn collect_user_service_problems(robot_root: &Path, robot: &Robot) -> Vec<String> {
    collect_user_service_dependency_report(robot_root, robot).problems
}

fn collect_user_service_dependency_report(
    robot_root: &Path,
    robot: &Robot,
) -> UserServiceDependencyReport {
    let mut report = UserServiceDependencyReport::default();
    for (name, runtime) in &robot.services {
        let runtime_dir = resolve_robot_path(robot_root, &runtime.path);
        let manifest_path = runtime_dir.join("Cargo.toml");
        let Ok(contents) = fs::read_to_string(&manifest_path) else {
            report.problems.push(format!(
                "user service '{name}' has no readable Cargo.toml at {}; cannot check phoxal dependency",
                manifest_path.display()
            ));
            continue;
        };
        let Ok(manifest) = toml::from_str::<TomlValue>(&contents) else {
            report.problems.push(format!(
                "user service '{name}' has an unparsable Cargo.toml at {}; cannot check phoxal dependency",
                manifest_path.display()
            ));
            continue;
        };
        let Some(dep) = manifest
            .get("dependencies")
            .and_then(|dependencies| dependencies.get("phoxal"))
        else {
            report.problems.push(format!(
                "user service '{name}' is missing a phoxal dependency; add a phoxal dependency"
            ));
            continue;
        };

        match phoxal_dependency(dep) {
            PhoxalDependency::Branch(branch) => report.problems.push(format!(
                "user service '{name}' floats on branch '{branch}'; pin it to a released phoxal version or tag"
            )),
            PhoxalDependency::Pinned(version) => report.successes.push(format!(
                "user service '{name}' declares phoxal dependency {version}"
            )),
            PhoxalDependency::Unparsable => report.problems.push(format!(
                "user service '{name}' has an unparsable phoxal dependency; pin it to a released phoxal version or tag"
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

fn print_text_report(robot: &Robot, suite: Option<&phoxal_cli_core::project::suite::Suite>) {
    println!("robot: {}", robot.robot.id);
    println!(
        "train: {}",
        suite.map_or("unavailable", |suite| suite.version.as_str())
    );
    println!("platform_services:");
    for artifact in suite
        .into_iter()
        .flat_map(|suite| suite.artifacts.iter())
        .filter(|artifact| artifact.kind == phoxal_cli_core::project::suite::Kind::Service)
    {
        println!(
            "  - {} -> {}",
            artifact.id,
            suite.map_or("unavailable", |suite| suite.version.as_str())
        );
    }
    println!("services:");
    for (name, runtime) in &robot.services {
        println!("  - {} -> {}", name, runtime.path.display());
    }
    println!("components:");
    for (instance_name, instance) in &robot.robot.components {
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

fn print_json_report(
    robot: &Robot,
    suite: Option<&phoxal_cli_core::project::suite::Suite>,
) -> Result<()> {
    let report = serde_json::json!({
        "robot": robot.robot.id,
        "train": suite.map(|suite| suite.version.clone()),
        "platform_services": suite.into_iter().flat_map(|suite| suite.artifacts.iter()).filter(|artifact| artifact.kind == phoxal_cli_core::project::suite::Kind::Service).map(|artifact| {
            let version = suite.map(|suite| suite.version.clone());
            serde_json::json!({
                "name": artifact.id,
                "version": version,
                "found": true,
            })
        }).collect::<Vec<_>>(),
        "services": robot.services.iter().map(|(name, runtime)| {
            serde_json::json!({
                "name": name,
                "path": runtime.path,
            })
        }).collect::<Vec<_>>(),
        "components": robot.robot.components.iter().map(|(instance_name, instance)| {
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
    fn pinned_user_service_phoxal_dep_collects_no_problems() -> anyhow::Result<()> {
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
        let robot = robot_with_user_service("runtimes/drive")?;

        let problems = collect_user_service_problems(temp.path(), &robot);

        assert!(problems.is_empty(), "unexpected problems: {problems:?}");
        Ok(())
    }

    #[test]
    fn branch_user_service_phoxal_dep_collects_problem() -> anyhow::Result<()> {
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
        let robot = robot_with_user_service("runtimes/drive")?;

        let problems = collect_user_service_problems(temp.path(), &robot);

        assert_eq!(
            problems,
            vec![
                "user service 'drive' floats on branch 'main'; pin it to a released phoxal version or tag"
                    .to_string()
            ]
        );
        Ok(())
    }

    #[test]
    fn missing_user_service_phoxal_dep_collects_problem() -> anyhow::Result<()> {
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
        let robot = robot_with_user_service("runtimes/drive")?;

        let problems = collect_user_service_problems(temp.path(), &robot);

        assert_eq!(
            problems,
            vec![
                "user service 'drive' is missing a phoxal dependency; add a phoxal dependency"
                    .to_string()
            ]
        );
        Ok(())
    }

    #[test]
    fn missing_user_service_manifest_collects_problem() -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        let runtime_dir = temp.path().join("runtimes/drive");
        let robot = robot_with_user_service("runtimes/drive")?;

        let problems = collect_user_service_problems(temp.path(), &robot);

        assert_eq!(
            problems,
            vec![format!(
                "user service 'drive' has no readable Cargo.toml at {}; cannot check phoxal dependency",
                runtime_dir.join("Cargo.toml").display()
            )]
        );
        Ok(())
    }

    fn write_manifest(runtime_dir: &Path, manifest: &str) -> anyhow::Result<()> {
        fs::create_dir_all(runtime_dir)?;
        fs::write(runtime_dir.join("Cargo.toml"), manifest)?;
        Ok(())
    }

    fn robot_with_user_service(runtime_path: &str) -> anyhow::Result<Robot> {
        Robot::parse_from_string(&format!(
            r#"schema: robot/v0
robot:
  id: testbot
  namespace: test
  motion_limits:
    max_linear_speed_mps: 0.6
    max_angular_speed_radps: 2.0
  kinematic:
    kind: differential
    left_actuators: [left_drive.motor]
    right_actuators: [right_drive.motor]
    left_encoders: [left_drive.encoder]
    right_encoders: [right_drive.encoder]
    wheel_radius_m: 0.1
    wheel_base_m: 0.5
  components:
    left_drive:
      component: ddsm115
      mount_link: left_wheel_mount
    right_drive:
      component: ddsm115
      mount_link: right_wheel_mount
artifacts: {{}}
services:
  drive:
    path: {runtime_path}
"#
        ))
    }
}
