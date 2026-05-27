use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};
use phoxal_cli_core::AppContext;

pub mod create;
pub mod doctor;
pub mod simulate;
pub mod validate;

#[derive(Debug, Parser)]
#[command(
    name = "phoxal-cli",
    about = "Resolve, validate, simulate, doctor, and scaffold Phoxal robot projects."
)]
pub struct Cli {
    #[arg(
        long = "project-path",
        global = true,
        help = "Project path used for robot.yaml discovery. Defaults to current directory."
    )]
    pub project_path: Option<PathBuf>,

    #[command(subcommand)]
    pub command: RootCommand,
}

#[derive(Debug, Subcommand)]
pub enum RootCommand {
    #[command(about = "Validate the robot project discovered from robot.yaml.")]
    Validate(validate::Validate),
    #[command(about = "Resolve and launch the local Webots simulation stack.")]
    Simulate(simulate::Simulate),
    #[command(about = "Check host prerequisites and pinned Phoxal tool binaries.")]
    Doctor(doctor::Doctor),
    #[command(subcommand, about = "Scaffold robot, runtime, and component files.")]
    Create(create::Create),
}

impl RootCommand {
    pub async fn run(&self, app: &AppContext) -> Result<()> {
        match self {
            Self::Validate(command) => command.run(app).await,
            Self::Simulate(command) => command.run(app).await,
            Self::Doctor(command) => command.run(app).await,
            Self::Create(command) => command.run(app).await,
        }
    }
}

pub async fn dispatch(cli: Cli, app: &AppContext) -> Result<()> {
    cli.command.run(app).await
}
