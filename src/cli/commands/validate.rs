//! Thin `phoxal validate` command adapter.

use anyhow::Result;
use clap::{Args, ValueEnum};

use crate::cli::AppContext;

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
        let report = crate::application::run::validate_command(app).await?;
        app.ui.success(format!(
            "validated {} with {} official services",
            report.robot_path.display(),
            report.platform_services.len()
        ));
        if self.report {
            match self.report_format {
                ReportFormat::Json => {
                    let json = serde_json::json!({
                        "robot": report.robot,
                        "train": report.train,
                        "platform_services": report.platform_services.iter().map(|name| {
                            serde_json::json!({
                                "name": name,
                                "version": report.train,
                                "found": true,
                            })
                        }).collect::<Vec<_>>(),
                        "services": report.services.iter().map(|name| {
                            serde_json::json!({"name": name, "declared": true})
                        }).collect::<Vec<_>>(),
                        "components": report.components,
                    });
                    println!("{}", serde_json::to_string_pretty(&json)?);
                }
                ReportFormat::Text => {
                    println!("robot: {}", report.robot);
                    println!("train: {}", report.train);
                    println!("platform_services:");
                    for service in &report.platform_services {
                        println!("  - {service} -> {}", report.train);
                    }
                    println!("services:");
                    for service in &report.services {
                        println!("  - {service} (declared)");
                    }
                    println!("components:");
                    for component in &report.components {
                        println!(
                            "  - {} ({}) from {}",
                            component.instance,
                            if component.has_driver {
                                "driver"
                            } else {
                                "no-driver"
                            },
                            component.source
                        );
                    }
                }
            }
        }
        Ok(())
    }
}
