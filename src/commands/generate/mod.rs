use std::fs;

use anyhow::Result;
use async_trait::async_trait;
use clap::Subcommand;

use crate::commands::{Command, ContainerSelectionArgs, GenerateComposeMode};
use phoxal_cli_core::AppContext;
use phoxal_cli_core::unit::Unit;
use phoxal_cli_core::unit::compose::{ComposeMode, GenerateCompose};
use phoxal_cli_core::unit::container::{plan_deploy_topology, plan_local_topology};
use phoxal_cli_core::unit::robot::ValidatedRobot;
use phoxal_engine::RobotIdentity;
use phoxal_utils_conventions::DEFAULT_ROBOT_NAMESPACE;

pub(crate) mod bundle;
pub(crate) mod compose;
#[derive(Debug, Subcommand)]
pub(crate) enum Generate {
    #[command(about = "Create a deterministic robot-model bundle.")]
    Bundle(bundle::Bundle),
    #[command(about = "Generate docker-compose.yml for a robot model.")]
    Compose(compose::Compose),
    #[command(about = "Generate the bundle and compose for a robot model.")]
    All(compose::Compose),
}

#[async_trait(?Send)]
impl Command for Generate {
    async fn execute(&self, app: &AppContext) -> Result<()> {
        match self {
            Self::Bundle(command) => command.run(app).await,
            Self::Compose(command) => command.run(app).await,
            Self::All(command) => command.run(app).await,
        }
    }
}

pub(super) fn write_compose(
    app: &AppContext,
    robot_model: &str,
    configuration: phoxal_engine::staged::Robot,
    mode: GenerateComposeMode,
    container_selection: ContainerSelectionArgs,
    output_path: Option<std::path::PathBuf>,
) -> Result<()> {
    let local_identity =
        RobotIdentity::new(robot_model.to_string(), DEFAULT_ROBOT_NAMESPACE.to_string());
    let selection = match mode {
        GenerateComposeMode::Deploy => container_selection
            .into_params()
            .resolve_for_production(&app.project, &configuration)?,
        GenerateComposeMode::Local => container_selection.into_params().resolve_for_local(),
    };
    let compose = GenerateCompose {
        topology: match mode {
            GenerateComposeMode::Deploy => plan_deploy_topology(
                &app.project,
                robot_model,
                &configuration,
                &selection,
                "ghcr.io/jbernavaprah",
                "latest",
            )?,
            GenerateComposeMode::Local => {
                // The localize image follows the robot's resolved backend roles.
                plan_local_topology(&app.project, robot_model, &configuration, &selection)?
            }
        },
        mode: match mode {
            GenerateComposeMode::Deploy => ComposeMode::Deploy,
            GenerateComposeMode::Local => ComposeMode::Local {
                identity: local_identity.clone(),
                rust_log: "info".to_string(),
            },
        },
    };
    let output_path = output_path.unwrap_or_else(|| match mode {
        GenerateComposeMode::Deploy => app.project.model_compose_path(robot_model),
        GenerateComposeMode::Local => app.project.dev_compose_path(robot_model),
    });
    let compose_content = compose.run(app)?;
    if let Some(parent) = output_path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&output_path, compose_content)?;
    app.ui
        .success(format!("Generated compose file: {}", output_path.display()));
    Ok(())
}

pub(super) fn generate_bundle_and_compose(
    app: &AppContext,
    robot_model: &str,
    mode: GenerateComposeMode,
    container_selection: ContainerSelectionArgs,
    output_path: Option<std::path::PathBuf>,
) -> Result<()> {
    let robot = app
        .ui
        .step("Validate Robot", || ValidatedRobot::load(app, robot_model))?;
    let configuration = robot.stage_bundle(app, None)?;
    write_compose(
        app,
        robot_model,
        configuration,
        mode,
        container_selection,
        output_path,
    )
}
