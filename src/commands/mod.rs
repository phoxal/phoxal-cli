use std::path::PathBuf;

use anyhow::Result;
use clap::{Args, Parser, Subcommand, ValueEnum};

use phoxal_cli_core::AppContext;
pub(crate) use phoxal_cli_core::Command;
use phoxal_cli_core::unit::container::{BuildPlatform, ContainerSelectionParams};
use phoxal_cli_webots::Webots;

pub(crate) mod deploy;
pub(crate) mod doctor;
pub(crate) mod generate;
pub(crate) mod report;
pub(crate) mod validate;

#[derive(Debug, Parser)]
#[command(
    name = "xtask",
    about = "Workspace orchestration for bundle, validate, image assembly, and runtime lifecycle.",
    long_about = "Developer entry point for robot framework workflows. Commands validate robot source configs and orchestrate local bundle, runtime image, and simulation operations."
)]
pub(crate) struct Cli {
    #[arg(
        long = "project-path",
        global = true,
        help = "Project path used as workspace root. Defaults to current directory."
    )]
    pub(crate) project_path: Option<PathBuf>,

    #[command(subcommand)]
    pub(crate) command: RootCommand,
}

#[derive(Debug, Subcommand)]
pub(crate) enum RootCommand {
    #[command(subcommand, about = "Generate bundle and compose artifacts.")]
    Generate(generate::Generate),
    #[command(subcommand, about = "Validate source robot or component files.")]
    Validate(validate::Validate),
    #[command(subcommand, about = "Print blueprint preflight reports.")]
    Report(report::Report),
    #[command(subcommand, about = "Build, publish, and deploy a robot model.")]
    Deploy(deploy::Deploy),
    #[command(about = "Check xtask host prerequisites and optionally install known missing ones.")]
    Doctor(doctor::Doctor),
    #[command(about = "Run the local webots simulator workflow.")]
    Webots(Webots),
}

#[derive(Debug, Copy, Clone, ValueEnum)]
pub(crate) enum GenerateComposeMode {
    Deploy,
    Local,
}

#[derive(Debug, Args, Clone, Default)]
pub(crate) struct ContainerSelectionArgs {
    #[arg(
        long = "enable-component-drivers",
        help = "Enable all component driver services."
    )]
    pub(crate) enable_component_drivers: bool,
    #[arg(
        long = "enable-component-driver",
        help = "Enable only component driver services matching the component type. Can be specified multiple times."
    )]
    pub(crate) enable_component_driver: Vec<String>,
    #[arg(
        long = "enable-component-driver-id",
        help = "Enable only a component driver service by component instance id. Can be specified multiple times."
    )]
    pub(crate) enable_component_driver_id: Vec<String>,
}

#[derive(Debug, Args, Clone)]
pub(crate) struct PlatformArgs {
    #[arg(
        long,
        help = "Rust target triple used to build images. Defaults to local or aarch64-unknown-linux-gnu based on command."
    )]
    pub(crate) target_triple: Option<String>,
    #[arg(
        long,
        help = "Docker platform used to build images. Defaults to local or linux/arm64 based on command."
    )]
    pub(crate) image_platform: Option<String>,
}

impl RootCommand {
    pub(crate) async fn run(&self, app: &AppContext) -> Result<()> {
        match self {
            Self::Generate(command) => command.run(app).await,
            Self::Validate(command) => command.run(app).await,
            Self::Report(command) => command.run(app).await,
            Self::Deploy(command) => command.run(app).await,
            Self::Doctor(command) => command.run(app).await,
            Self::Webots(command) => command.run(app).await,
        }
    }
}

pub(crate) async fn dispatch(cli: Cli, app: &AppContext) -> Result<()> {
    cli.command.run(app).await
}

impl PlatformArgs {
    pub(crate) fn into_build_platform(self, default_platform: BuildPlatform) -> BuildPlatform {
        BuildPlatform::new(
            self.target_triple
                .unwrap_or_else(|| default_platform.target_triple.clone()),
            self.image_platform
                .unwrap_or_else(|| default_platform.docker_platform.clone()),
        )
    }
}

impl ContainerSelectionArgs {
    pub(crate) fn into_params(self) -> ContainerSelectionParams {
        ContainerSelectionParams {
            enable_component_drivers: self.enable_component_drivers,
            enable_component_driver: self.enable_component_driver,
            enable_component_driver_id: self.enable_component_driver_id,
        }
    }
}
