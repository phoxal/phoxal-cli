use std::path::Path;

use anyhow::{Context, Result};
use clap::{Args, Subcommand};
use serde::Serialize;

use crate::AppContext;
use phoxal_cli_core::project::resolver::{discover_robot_yaml, load_robot_with_extras};

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
pub struct Catalog {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ServiceCatalogSummary {
    pub entries: Vec<ServiceCatalogEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ServiceCatalogEntry {
    pub id: String,
    pub versions: Vec<String>,
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
        for entry in &summary.entries {
            println!(
                "{} -> versions [{}] ({})",
                entry.id,
                entry.versions.join(", "),
                entry.participant_kind
            );
        }
        Ok(())
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
        loaded.robot.artifacts.channel,
        &loaded.extras,
    )?
    .ok_or_else(|| anyhow::anyhow!("artifact catalog unavailable"))?;
    Ok(ServiceCatalogSummary {
        entries: phoxal_cli_core::project::catalog::OFFICIAL_SERVICES
            .iter()
            .map(|(_, package)| ServiceCatalogEntry {
                id: (*package).to_string(),
                versions: catalog
                    .artifacts
                    .iter()
                    .filter(|entry| entry.package == *package)
                    .map(|entry| entry.version.clone())
                    .collect(),
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
    use phoxal_cli_core::project::catalog::{
        SelectionChannel as CatalogChannel, fixture_catalog_for_tests, fixture_contract_for_tests,
        fixture_service_entry_for_tests,
    };

    #[test]
    fn service_catalog_summary_lists_official_services() -> Result<()> {
        let temp = tempfile::tempdir()?;
        fs::write(temp.path().join("robot.yaml"), minimal_robot_yaml())?;
        let catalog = write_catalog(temp.path())?;

        let summary = service_catalog_summary(temp.path(), Some(catalog))?;

        assert_eq!(
            summary.entries.len(),
            phoxal_cli_core::project::catalog::OFFICIAL_SERVICES.len()
        );
        let entry = summary
            .entries
            .iter()
            .find(|entry| entry.id == "phoxal/service-drive")
            .expect("drive is part of the platform model");
        assert_eq!(entry.id, "phoxal/service-drive");
        assert_eq!(entry.versions, vec!["0.1.0".to_string()]);
        assert_eq!(entry.participant_kind, "service");

        Ok(())
    }

    fn minimal_robot_yaml() -> &'static str {
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
  components: {}
artifacts:
  channel: stable
"#
    }

    fn write_catalog(root: &Path) -> Result<String> {
        let catalog = fixture_catalog_for_tests(vec![fixture_service_entry_for_tests(
            "drive",
            "0.1.0",
            CatalogChannel::Stable,
            &crate::resolver::host_target_triple(),
            false,
            vec![fixture_contract_for_tests("v1::drive::Target", "publish")],
        )]);
        let path = root.join("catalog.json");
        fs::write(&path, serde_json::to_string_pretty(&catalog)?)?;
        Ok(path.display().to_string())
    }
}
