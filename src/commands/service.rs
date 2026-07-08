use std::path::Path;

use anyhow::{Context, Result};
use clap::{Args, Subcommand};
use serde::Serialize;

use crate::AppContext;
use crate::commands::MessageFormat;
use crate::resolver::{discover_robot_yaml, load_robot_with_extras};

#[derive(Debug, Args)]
pub struct Service {
    #[command(subcommand)]
    pub command: ServiceSubcommand,
}

#[derive(Debug, Subcommand)]
pub enum ServiceSubcommand {
    #[command(about = "Print official services from the configured artifact catalog.")]
    Catalog(Catalog),
}

#[derive(Debug, Args)]
pub struct Catalog {
    #[arg(
        long,
        value_enum,
        default_value_t = MessageFormat::Human,
        help = "Output format for the catalog listing."
    )]
    pub message_format: MessageFormat,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ServiceCatalogSummary {
    pub entries: Vec<ServiceCatalogEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ServiceCatalogEntry {
    pub id: String,
    pub api_generations: Vec<String>,
    pub participant_kind: &'static str,
}

impl Service {
    pub async fn run(&self, app: &AppContext) -> Result<()> {
        match &self.command {
            ServiceSubcommand::Catalog(command) => command.run(app).await,
        }
    }
}

impl Catalog {
    pub async fn run(&self, app: &AppContext) -> Result<()> {
        let root = app.project.root().to_path_buf();
        let catalog_source = app.catalog_source.clone();
        let summary =
            tokio::task::spawn_blocking(move || service_catalog_summary(&root, catalog_source))
                .await
                .context("service catalog worker failed")??;
        crate::commands::print_message(
            &summary,
            || {
                for entry in &summary.entries {
                    println!(
                        "{} -> api_generations [{}] ({})",
                        entry.id,
                        entry.api_generations.join(", "),
                        entry.participant_kind
                    );
                }
                Ok(())
            },
            self.message_format,
        )
    }
}

pub fn service_catalog_summary(
    project_start: &Path,
    catalog_source: Option<String>,
) -> Result<ServiceCatalogSummary> {
    let robot_path = discover_robot_yaml(project_start)
        .with_context(|| format!("failed to find robot.yaml from {}", project_start.display()))?;
    let project_root = robot_path
        .parent()
        .context("robot.yaml did not have a parent directory")?;
    let loaded = load_robot_with_extras(&robot_path)?;
    let catalog = crate::commands::load_catalog_for_robot_from_source(
        catalog_source,
        project_root,
        &loaded.extras,
        false,
    )?
    .ok_or_else(crate::catalog::unavailable_catalog_error)?;
    Ok(ServiceCatalogSummary {
        entries: catalog
            .services
            .iter()
            .map(|entry| ServiceCatalogEntry {
                id: entry.package.clone(),
                api_generations: vec![entry.api_generation.clone()],
                participant_kind: "service",
            })
            .collect(),
    })
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use super::*;
    use crate::catalog::{
        Channel as CatalogChannel, fixture_catalog_for_tests, fixture_contract_for_tests,
        fixture_service_entry_for_tests,
    };

    #[test]
    fn service_catalog_summary_lists_official_services() -> Result<()> {
        let temp = tempfile::tempdir()?;
        fs::write(temp.path().join("robot.yaml"), minimal_robot_yaml())?;
        let catalog = write_catalog(temp.path())?;

        let summary = service_catalog_summary(temp.path(), Some(catalog))?;

        assert_eq!(summary.entries.len(), 1);
        let entry = &summary.entries[0];
        assert_eq!(entry.id, "phoxal/service-drive");
        assert_eq!(entry.api_generations, vec!["y2026_1".to_string()]);
        assert_eq!(entry.participant_kind, "service");

        Ok(())
    }

    fn minimal_robot_yaml() -> &'static str {
        r#"schema: v0
robot:
  id: testbot
  namespace: test
  kinematic:
    kind: differential
    left_actuators: [left_drive.motor]
    right_actuators: [right_drive.motor]
    left_encoders: [left_drive.encoder]
    right_encoders: [right_drive.encoder]
    wheel_radius_m: 0.1
    wheel_base_m: 0.5
  components: {}
artifacts:
  channel: stable
"#
    }

    fn write_catalog(root: &Path) -> Result<String> {
        let catalog = fixture_catalog_for_tests(vec![fixture_service_entry_for_tests(
            "drive",
            "y2026_1",
            "0.1.0",
            CatalogChannel::Stable,
            &crate::resolver::host_target_triple(),
            false,
            vec![fixture_contract_for_tests(
                "drive::Target",
                "0123456789abcdef",
            )],
        )]);
        let path = root.join("catalog.json");
        fs::write(&path, serde_json::to_string_pretty(&catalog)?)?;
        Ok(path.display().to_string())
    }
}
