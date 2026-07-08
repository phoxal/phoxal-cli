use std::collections::BTreeSet;

use anyhow::{Context, Result};
use clap::{Args, Subcommand};
use serde::Serialize;

use crate::AppContext;
use crate::commands::MessageFormat;

#[derive(Debug, Args)]
pub struct Generations {
    #[command(subcommand)]
    pub command: GenerationsSubcommand,
}

#[derive(Debug, Subcommand)]
pub enum GenerationsSubcommand {
    #[command(about = "Report API generation readiness from the artifact catalog.")]
    Status(Status),
}

#[derive(Debug, Args)]
pub struct Status {
    #[arg(long, value_name = "GENERATION", help = "Generation to inspect.")]
    pub generation: Option<String>,
    #[arg(
        long,
        help = "Refresh the artifact catalog before reporting readiness."
    )]
    pub pull: bool,
    #[arg(long, value_enum, default_value_t = MessageFormat::Human)]
    pub message_format: MessageFormat,
}

#[derive(Debug, Clone, Serialize)]
pub struct GenerationStatusOutput {
    pub generation: String,
    pub channel: String,
    pub target: String,
    pub catalog_revision: String,
    pub ready: bool,
    pub changed_contract_count: usize,
    pub changed_contracts: Vec<String>,
    pub artifacts: Vec<ArtifactReadiness>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ArtifactReadiness {
    pub artifact_id: String,
    pub kind: String,
    pub version: String,
    pub api_generation: String,
    pub target_status: String,
    pub per_triple_status: Vec<TripleStatus>,
    pub changed_contracts: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TripleStatus {
    pub triple: String,
    pub status: String,
}

impl Generations {
    pub async fn run(&self, app: &AppContext) -> Result<()> {
        match &self.command {
            GenerationsSubcommand::Status(command) => command.run(app).await,
        }
    }
}

impl Status {
    pub async fn run(&self, app: &AppContext) -> Result<()> {
        let output = status(app, self.generation.as_deref(), self.pull)?;
        crate::commands::print_message(
            &output,
            || {
                println!(
                    "generation {} on {} for {}: {}",
                    output.generation,
                    output.channel,
                    output.target,
                    if output.ready { "ready" } else { "not ready" }
                );
                println!("catalog revision: {}", output.catalog_revision);
                println!(
                    "changed contracts in this robot scope: {}",
                    output.changed_contract_count
                );
                for contract in &output.changed_contracts {
                    println!("  - {contract}");
                }
                println!("artifacts:");
                for artifact in &output.artifacts {
                    println!(
                        "  - {} {} ({}) -> {}",
                        artifact.kind,
                        artifact.artifact_id,
                        artifact.version,
                        artifact.target_status
                    );
                    if !artifact.changed_contracts.is_empty() {
                        println!(
                            "    changed contracts: {}",
                            artifact.changed_contracts.join(", ")
                        );
                    }
                    if !artifact.per_triple_status.is_empty() {
                        let statuses = artifact
                            .per_triple_status
                            .iter()
                            .map(|status| format!("{}: {}", status.triple, status.status))
                            .collect::<Vec<_>>()
                            .join(", ");
                        println!("    triples: {statuses}");
                    }
                }
                Ok(())
            },
            self.message_format,
        )
    }
}

pub fn status(
    app: &AppContext,
    generation: Option<&str>,
    refresh: bool,
) -> Result<GenerationStatusOutput> {
    let robot_path = crate::resolver::discover_robot_yaml(app.project.root())?;
    let project_root = robot_path
        .parent()
        .context("robot.yaml did not have a parent directory")?;
    let loaded = crate::resolver::load_robot_with_extras(&robot_path)?;
    let robot = loaded.robot;
    let catalog =
        crate::commands::load_catalog_for_robot(app, project_root, &loaded.extras, refresh)?
            .ok_or_else(|| {
                crate::native_pending::error(
                    "generation readiness from the generated artifact catalog (06)",
                )
            })?;
    let target = crate::resolver::host_target_triple();
    let generation = generation
        .map(str::to_string)
        .map(Ok)
        .unwrap_or_else(|| crate::resolver::target_generation_for_robot(&robot, Some(&catalog)))?;
    let channel = crate::catalog::to_catalog_channel(robot.artifacts.channel);

    let entries = catalog
        .services
        .iter()
        .map(|entry| (crate::catalog::ArtifactKind::Service, entry))
        .chain(
            catalog
                .drivers
                .iter()
                .map(|entry| (crate::catalog::ArtifactKind::ComponentDriver, entry)),
        )
        .chain(
            catalog
                .tools
                .iter()
                .map(|entry| (crate::catalog::ArtifactKind::Tool, entry)),
        )
        .chain(
            catalog
                .simulators
                .iter()
                .map(|entry| (crate::catalog::ArtifactKind::Simulator, entry)),
        )
        .filter(|(_, entry)| entry.api_generation == generation)
        .filter(|(_, entry)| entry.channels.contains_key(&channel))
        .collect::<Vec<_>>();
    let robot_scoped_artifact_ids = robot_scoped_artifact_ids(&robot, &entries);

    let mut artifacts = entries
        .iter()
        .map(|(kind, entry)| {
            let published = entry.artifacts.contains_key(&target);
            let target_status = if published { "released" } else { "missing" }.to_string();
            let per_triple_status = entry
                .artifacts
                .keys()
                .map(|triple| TripleStatus {
                    triple: triple.clone(),
                    status: "released".to_string(),
                })
                .collect::<Vec<_>>();
            ArtifactReadiness {
                artifact_id: entry.package.clone(),
                kind: kind.to_string(),
                version: entry.version.clone(),
                api_generation: entry.api_generation.clone(),
                target_status,
                per_triple_status,
                changed_contracts: entry.changed_contracts.clone(),
            }
        })
        .collect::<Vec<_>>();
    artifacts.sort_by(|left, right| left.artifact_id.cmp(&right.artifact_id));

    let ready = !artifacts.is_empty()
        && artifacts
            .iter()
            .all(|artifact| artifact.target_status == "released");
    let mut changed_contracts = BTreeSet::new();
    for artifact in artifacts
        .iter()
        .filter(|artifact| robot_scoped_artifact_ids.contains(&artifact.artifact_id))
    {
        changed_contracts.extend(artifact.changed_contracts.iter().cloned());
    }
    let changed_contracts = changed_contracts.into_iter().collect::<Vec<_>>();

    Ok(GenerationStatusOutput {
        generation,
        channel: crate::catalog::channel_as_str(channel).to_string(),
        target,
        catalog_revision: catalog.revision,
        ready,
        changed_contract_count: changed_contracts.len(),
        changed_contracts,
        artifacts,
    })
}

fn robot_scoped_artifact_ids(
    robot: &phoxal::model::robot::RobotV1,
    entries: &[(crate::catalog::ArtifactKind, &crate::catalog::ArtifactEntry)],
) -> BTreeSet<String> {
    let driver_component_ids = robot
        .robot
        .components
        .values()
        .filter(|instance| instance.driver.is_some())
        .map(|instance| instance.component.as_str())
        .collect::<BTreeSet<_>>();

    entries
        .iter()
        .filter_map(|(kind, entry)| match kind {
            crate::catalog::ArtifactKind::Service => Some(entry.package.clone()),
            crate::catalog::ArtifactKind::ComponentDriver
                if crate::catalog::artifact_name(*kind, &entry.package)
                    .is_some_and(|name| driver_component_ids.contains(name)) =>
            {
                Some(entry.package.clone())
            }
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::catalog::{
        Channel as CatalogChannel, fixture_catalog_for_tests,
        fixture_component_driver_entry_for_tests, fixture_contract_for_tests,
        fixture_service_entry_for_tests,
    };

    #[test]
    fn status_excludes_unused_driver_contracts_from_robot_scope() -> Result<()> {
        let temp = tempfile::tempdir()?;
        fs::write(temp.path().join("robot.yaml"), driverless_robot_yaml())?;
        fs::write(temp.path().join("catalog.json"), catalog_json()?)?;
        let app = AppContext::new(
            temp.path().to_path_buf(),
            Some(temp.path().join("catalog.json").display().to_string()),
        )?;

        let output = status(&app, None, false)?;

        assert!(
            output
                .artifacts
                .iter()
                .any(|artifact| artifact.artifact_id == "phoxal/component-bno085-driver"),
            "driver rows stay visible in the whole-catalog readiness table"
        );
        assert_eq!(output.changed_contracts, vec!["drive::Target"]);
        Ok(())
    }

    #[test]
    fn status_includes_used_component_driver_contracts_by_artifact_name() -> Result<()> {
        let temp = tempfile::tempdir()?;
        fs::write(temp.path().join("robot.yaml"), robot_with_driver_yaml())?;
        fs::write(temp.path().join("catalog.json"), catalog_json()?)?;
        let app = AppContext::new(
            temp.path().to_path_buf(),
            Some(temp.path().join("catalog.json").display().to_string()),
        )?;

        let output = status(&app, None, false)?;

        assert_eq!(
            output.changed_contracts,
            vec!["drive::Target", "wheel::State"]
        );
        assert!(
            !output
                .changed_contracts
                .contains(&"imu::Reading".to_string())
        );
        Ok(())
    }

    fn catalog_json() -> Result<String> {
        let target = crate::resolver::host_target_triple();
        let mut service = fixture_service_entry_for_tests(
            "drive",
            "y2026_1",
            "0.1.0",
            CatalogChannel::Stable,
            &target,
            false,
            vec![fixture_contract_for_tests(
                "drive::Target",
                "0123456789abcdef",
            )],
        );
        service.as_artifact_entry_mut().changed_contracts = vec!["drive::Target".to_string()];

        let mut used_driver = fixture_component_driver_entry_for_tests(
            "ddsm115",
            "y2026_1",
            "0.1.0",
            CatalogChannel::Stable,
            &target,
            false,
            vec![fixture_contract_for_tests(
                "wheel::State",
                "fedcba9876543210",
            )],
        );
        used_driver.as_artifact_entry_mut().changed_contracts = vec!["wheel::State".to_string()];

        let mut unused_driver = fixture_component_driver_entry_for_tests(
            "bno085",
            "y2026_1",
            "0.1.0",
            CatalogChannel::Stable,
            &target,
            false,
            vec![fixture_contract_for_tests(
                "imu::Reading",
                "1111111111111111",
            )],
        );
        unused_driver.as_artifact_entry_mut().changed_contracts = vec!["imu::Reading".to_string()];

        let catalog = fixture_catalog_for_tests(vec![service, used_driver, unused_driver]);
        Ok(serde_json::to_string_pretty(&catalog)?)
    }

    fn driverless_robot_yaml() -> &'static str {
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
  components:
    left_drive:
      component: ddsm115
      mount_link: left_wheel_mount
artifacts:
  channel: stable
  generation: y2026_1
"#
    }

    fn robot_with_driver_yaml() -> &'static str {
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
  components:
    left_drive:
      component: ddsm115
      mount_link: left_wheel_mount
      driver:
        connection: { type: can, bus: 0, node_id: 1 }
artifacts:
  channel: stable
  generation: y2026_1
"#
    }
}
