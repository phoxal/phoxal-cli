use std::fs;

use anyhow::{Result, anyhow, bail};
use async_trait::async_trait;
use clap::Parser;

use crate::commands::{Command, ContainerSelectionArgs, PlatformArgs};
use phoxal_cli_core::AppContext;
use phoxal_cli_core::unit::Unit;
use phoxal_cli_core::unit::compose::{ComposeMode, GenerateCompose};
use phoxal_cli_core::unit::container::{BuildPlatform, plan_deploy_topology};
use phoxal_cli_core::unit::publish::{BuildImages, PushImages};

#[derive(Debug, Parser)]
pub(crate) struct Fleet {
    #[arg(help = "The robot model to deploy.")]
    pub(crate) robot_model: String,

    #[arg(help = "The target balena fleet name.")]
    pub(crate) fleet: String,

    #[arg(help = "The registry endpoint.")]
    pub(crate) registry: String,

    #[arg(help = "The tag to push.")]
    pub(crate) tag: String,

    #[command(flatten)]
    pub(crate) platform: PlatformArgs,

    #[command(flatten)]
    pub(crate) container_selection: ContainerSelectionArgs,

    #[arg(long, help = "Do not use Docker cache when building images.")]
    pub(crate) no_cache: bool,
}

#[async_trait(?Send)]
impl Command for Fleet {
    async fn execute(&self, app: &AppContext) -> Result<()> {
        app.ui.info(format!(
            "Deploying robot model {} to fleet {} via {} with tag {}",
            self.robot_model, self.fleet, self.registry, self.tag
        ));

        let configuration =
            phoxal_cli_core::unit::bundle::Bundle::new(self.robot_model.clone()).run(app)?;

        let build_platform = self
            .platform
            .clone()
            .into_build_platform(BuildPlatform::release_default());

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
        fs::write(&compose_path, compose_content)?;

        app.ui.info(format!(
            "Deploy images pushed to registry. Balena compose generated at {}",
            compose_path.display()
        ));
        let compose_dir = compose_path
            .parent()
            .ok_or_else(|| anyhow!("compose path '{}' has no parent", compose_path.display()))?;
        app.ui.info(format!("You can now deploy this to the fleet by running `balena deploy {} --build --source {}`", self.fleet, compose_dir.display()));

        // Actually deploying to fleet via balena cli or similar is usually a separate step or via balena deploy command here.
        // Assuming xtask's responsibility is generating the correct compose and pushing images.
        // Let's invoke balena deploy since it says "deploys that tagged payload to the fleet"

        let mut cmd = std::process::Command::new("balena");
        cmd.arg("deploy")
            .arg(&self.fleet)
            .arg("--source")
            .arg(compose_dir);
        let deploy_title = format!("Running balena deploy to fleet {}", self.fleet);
        app.ui.step(&deploy_title, || {
            let status = app.ui.command_status(&mut cmd)?;
            if !status.success() {
                bail!("balena deploy failed with status {status}");
            }
            Ok(())
        })?;

        Ok(())
    }
}
