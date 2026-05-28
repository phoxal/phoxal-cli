use anyhow::{Context, Result, anyhow};
use clap::{Args, ValueEnum};
use phoxal_utils_robot::Robot;
use semver::{Version, VersionReq};

use crate::AppContext;

use crate::catalog::CATALOG;

#[derive(Debug, Args)]
pub struct Validate {
    #[arg(long, help = "Print the derived runtime/component graph.")]
    pub report: bool,
    #[arg(long, value_enum, default_value_t = ReportFormat::Text)]
    pub report_format: ReportFormat,
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

fn join_errors(errors: Vec<phoxal_utils_robot::ValidationError>) -> String {
    errors
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n")
}
