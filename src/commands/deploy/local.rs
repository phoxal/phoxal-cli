use std::collections::BTreeMap;
use std::fs;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use clap::Parser;
use reqwest::blocking::Client;
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::commands::{Command, ContainerSelectionArgs, PlatformArgs};
use phoxal_cli_core::AppContext;
use phoxal_cli_core::unit::Unit;
use phoxal_cli_core::unit::compose::{ComposeMode, GenerateCompose};
use phoxal_cli_core::unit::container::{BuildPlatform, plan_deploy_topology};
use phoxal_cli_core::unit::publish::{BuildImages, PushImages};

#[derive(Debug, Parser)]
pub(crate) struct Local {
    #[arg(help = "The robot model to deploy.")]
    pub(crate) robot_model: String,

    #[arg(help = "The IP address of the target device.")]
    pub(crate) ip: String,

    #[arg(
        long,
        env = "LOCAL_REGISTRY",
        help = "The local registry endpoint. Required unless configured in environment."
    )]
    pub(crate) registry: String,

    #[arg(long, default_value = "local", help = "The tag to push.")]
    pub(crate) tag: String,

    #[command(flatten)]
    pub(crate) platform: PlatformArgs,

    #[command(flatten)]
    pub(crate) container_selection: ContainerSelectionArgs,

    #[arg(long, help = "Do not use Docker cache when building images.")]
    pub(crate) no_cache: bool,
}

#[async_trait(?Send)]
impl Command for Local {
    async fn execute(&self, app: &AppContext) -> Result<()> {
        app.ui.info(format!(
            "Deploying robot model {} to local device at {}",
            self.robot_model, self.ip
        ));

        let build_platform = self
            .platform
            .clone()
            .into_build_platform(BuildPlatform::release_default());

        let configuration =
            phoxal_cli_core::unit::bundle::Bundle::new(self.robot_model.clone()).run(app)?;

        let container_selection = self
            .container_selection
            .clone()
            .into_params()
            .resolve_for_production(&app.project, &configuration)?;

        let topology = plan_deploy_topology(
            &app.project,
            &self.robot_model,
            &configuration,
            &container_selection,
            &self.registry,
            &self.tag,
        )?;
        BuildImages {
            topology: topology.clone(),
            extra_images: Vec::new(),
            platform: build_platform,
            bundle_dir: app.project.bundle_dir(&self.robot_model),
            no_cache: self.no_cache,
        }
        .run(app)?;
        PushImages {
            topology: topology.clone(),
            extra_images: Vec::new(),
        }
        .run(app)?;
        let compose_path = app.project.model_compose_path(&self.robot_model);
        let compose_content = GenerateCompose {
            topology,
            mode: ComposeMode::Deploy,
        }
        .run(app)?;
        if let Some(parent) = compose_path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&compose_path, &compose_content)?;
        let payload = local_target_state_from_compose(&compose_content)
            .context("Failed to build local Supervisor target state from deploy compose")?;
        let supervisor_url = format!("http://{}:48484/v2/local/target-state", self.ip);

        app.ui.step("Deploying to local device", || {
            let client = Client::builder().timeout(Duration::from_secs(30)).build()?;
            let response = client
                .post(&supervisor_url)
                .json(&payload)
                .send()
                .with_context(|| {
                    format!(
                        "Failed to send request to Supervisor API at {}",
                        supervisor_url
                    )
                })?;

            if !response.status().is_success() {
                bail!(
                    "Supervisor API returned error: {}",
                    response.text().unwrap_or_default()
                );
            }
            Ok(())
        })?;

        Ok(())
    }
}

fn local_target_state_from_compose(compose_content: &str) -> Result<Value> {
    let compose = serde_yaml::from_str::<DeployComposeDocument>(compose_content)?;
    let mut services = Map::new();

    for (index, (service_name, service)) in compose.services.into_iter().enumerate() {
        let service_id = (index + 1).to_string();
        services.insert(
            service_id,
            json!({
                "serviceName": service_name,
                "serviceId": index + 1,
                "imageId": index + 1,
                "image": service.image,
                "environment": service.environment.unwrap_or_default(),
                "labels": service.labels.unwrap_or_default(),
                "volumes": service.volumes.unwrap_or_default(),
                "devices": service.devices.unwrap_or_default(),
                "depends_on": service.depends_on.unwrap_or_default(),
                "network_mode": service.network_mode,
                "restart": service.restart,
                "command": service.command,
                "entrypoint": service.entrypoint,
                "cap_add": service.cap_add.unwrap_or_default(),
                "running": true,
            }),
        );
    }

    Ok(json!({
        "local": {
            "name": "local",
            "config": {},
            "apps": {
                "1": {
                    "name": "local",
                    "commit": "local",
                    "releaseId": "1",
                    "services": services,
                    "volumes": compose.volumes,
                    "networks": compose.networks,
                }
            }
        }
    }))
}

#[derive(Debug, Deserialize)]
struct DeployComposeDocument {
    services: BTreeMap<String, DeployComposeService>,
    #[serde(default)]
    volumes: Map<String, Value>,
    #[serde(default)]
    networks: Map<String, Value>,
}

#[derive(Debug, Deserialize)]
struct DeployComposeService {
    image: String,
    #[serde(default)]
    environment: Option<Value>,
    #[serde(default)]
    labels: Option<Map<String, Value>>,
    #[serde(default)]
    volumes: Option<Vec<String>>,
    #[serde(default)]
    devices: Option<Vec<String>>,
    #[serde(default)]
    depends_on: Option<Vec<String>>,
    #[serde(default)]
    network_mode: Option<String>,
    #[serde(default)]
    restart: Option<String>,
    #[serde(default)]
    command: Option<Vec<String>>,
    #[serde(default)]
    entrypoint: Option<Vec<String>>,
    #[serde(default)]
    cap_add: Option<Vec<String>>,
}
