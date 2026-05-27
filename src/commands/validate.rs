use anyhow::{Result, anyhow};
use clap::{Args, ValueEnum};
use phoxal_cli_core::AppContext;

use crate::catalog::CATALOG;
use crate::resolver::{ResolveOptions, resolve};

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
        let resolved = resolve(
            &robot,
            &CATALOG,
            ResolveOptions {
                resolve_external_artifacts: false,
                ..ResolveOptions::default()
            },
        )?;
        app.ui.success(format!(
            "validated {} with {} platform runtimes",
            robot_path.display(),
            resolved.platform_runtimes.len()
        ));
        if self.report {
            match self.report_format {
                ReportFormat::Text => print_text_report(&resolved),
                ReportFormat::Json => print_json_report(&resolved)?,
            }
        }
        Ok(())
    }
}

fn print_text_report(resolved: &crate::resolver::ResolvedRobot) {
    println!("robot: {}", resolved.robot.identity.id);
    println!("runtime_set: {}", resolved.runtime_set_version);
    println!("platform_runtimes:");
    for runtime in &resolved.platform_runtimes {
        println!(
            "  - {} -> {}:{}",
            runtime.name, runtime.image_repo, runtime.version
        );
    }
    println!("user_runtimes:");
    for runtime in &resolved.user_runtimes {
        println!("  - {} -> {}", runtime.name, runtime.path.display());
    }
    println!("components:");
    for component in &resolved.components {
        let driver = if component.has_driver {
            "driver"
        } else {
            "no-driver"
        };
        println!(
            "  - {} ({}) from {}",
            component.instance, driver, component.source_name
        );
    }
}

fn print_json_report(resolved: &crate::resolver::ResolvedRobot) -> Result<()> {
    let report = serde_json::json!({
        "robot": resolved.robot.identity.id,
        "runtime_set": resolved.runtime_set_version.to_string(),
        "platform_runtimes": resolved.platform_runtimes.iter().map(|runtime| {
            serde_json::json!({
                "name": runtime.name,
                "image_repo": runtime.image_repo,
                "version": runtime.version.to_string(),
            })
        }).collect::<Vec<_>>(),
        "user_runtimes": resolved.user_runtimes.iter().map(|runtime| {
            serde_json::json!({
                "name": runtime.name,
                "path": runtime.path,
            })
        }).collect::<Vec<_>>(),
        "components": resolved.components.iter().map(|component| {
            serde_json::json!({
                "instance": component.instance,
                "source": component.source_name,
                "has_driver": component.has_driver,
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
