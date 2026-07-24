use std::path::Path;

use anyhow::{Context, Result, anyhow};
use clap::{Args, ValueEnum};
use phoxal::model::robot::RobotV0 as Robot;

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
        let robot = phoxal_cli_core::project::resolver::load_robot(&robot_path)?;
        let suite_source = app.suite_source.clone();
        let suite_project_root = project_root.to_path_buf();
        let suite = tokio::task::spawn_blocking(move || {
            crate::commands::load_suite_for_robot_from_source(suite_source, &suite_project_root)
        })
        .await
        .context("suite loader failed")??;
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
        let workspace_problems = check_workspace_runtimes(app, &robot_path, &robot)?;
        anyhow::ensure!(
            workspace_problems.is_empty(),
            "Cargo workspace runtime discovery failed:\n{}",
            workspace_problems.join("\n")
        );
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

fn check_workspace_runtimes(
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

fn collect_user_service_dependency_report(
    robot_root: &Path,
    robot: &Robot,
) -> UserServiceDependencyReport {
    use phoxal_cli_core::project::train::WorkspaceRuntimeKind;

    let mut report = UserServiceDependencyReport::default();
    // The declaration-only invariants first, matching resolution's ordering
    // (#950): dual names and official identities fail before any workspace
    // reasoning.
    if let Err(error) = phoxal_cli_core::project::layout::validate_runtime_declarations(robot) {
        report.problems.push(format!("{error:#}"));
        return report;
    }
    let project = match phoxal_cli_core::project::train::resolve_locked_project(robot_root) {
        Ok(project) => project,
        Err(error) => {
            report
                .problems
                .push(format!("locked Cargo workspace is invalid: {error:#}"));
            return report;
        }
    };
    let discovered = |kind: WorkspaceRuntimeKind| {
        project
            .runtimes
            .iter()
            .filter(move |runtime| runtime.kind == kind)
            .filter_map(|runtime| {
                runtime
                    .crate_dir
                    .file_name()
                    .and_then(|name| name.to_str())
                    .map(str::to_string)
            })
            .collect::<std::collections::BTreeSet<_>>()
    };
    let services = discovered(WorkspaceRuntimeKind::Service);
    let tool_crates = discovered(WorkspaceRuntimeKind::Tool);
    for service in &services {
        let note = if robot.services.contains_key(service) {
            "declared"
        } else {
            "undeclared - not part of the robot"
        };
        report.successes.push(format!(
            "Cargo workspace service '{service}' discovered ({note})"
        ));
    }
    for configured in robot.services.keys() {
        if !services.contains(configured) {
            report.problems.push(format!(
                "services.{configured} has no matching services/{configured} workspace crate"
            ));
        }
    }
    for configured in robot.tools.keys() {
        if !tool_crates.contains(configured) {
            report.problems.push(format!(
                "tools.{configured} has no matching tools/{configured} workspace crate"
            ));
        }
    }
    report
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
    for name in robot.services.keys() {
        println!("  - {name} (declared)");
    }
    println!("tools:");
    for name in robot.tools.keys() {
        println!("  - {name} (declared)");
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
        "services": robot.services.keys().map(|name| {
            serde_json::json!({
                "name": name,
                "declared": true,
            })
        }).collect::<Vec<_>>(),
        "tools": robot.tools.keys().map(|name| {
            serde_json::json!({
                "name": name,
                "declared": true,
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
