use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};

use crate::AppContext;

pub mod doctor;
pub mod self_cmd;
pub mod simulate;
pub mod update;
pub mod validate;

#[derive(Debug, Parser)]
#[command(
    name = "phoxal-cli",
    version,
    about = "Resolve, validate, simulate, and doctor Phoxal robot projects."
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
    #[command(about = "Resolve robot.yaml and refresh phoxal.lock.")]
    Update(update::Update),
    #[command(about = "Resolve and launch the local Webots simulation stack.")]
    Simulate(simulate::Simulate),
    #[command(about = "Check host prerequisites without modifying the host or project.")]
    Doctor(doctor::Doctor),
    #[command(about = "Print the phoxal-cli version.")]
    Version,
    #[command(name = "self", about = "Manage this phoxal-cli installation.")]
    SelfCmd(self_cmd::SelfCmd),
}

impl RootCommand {
    pub async fn run(&self, app: &AppContext) -> Result<()> {
        match self {
            Self::Validate(command) => command.run(app).await,
            Self::Update(command) => command.run(app).await,
            Self::Simulate(command) => command.run(app).await,
            Self::Doctor(command) => command.run(app).await,
            Self::Version => {
                println!(
                    "phoxal-cli {} ({}-{})",
                    env!("CARGO_PKG_VERSION"),
                    std::env::consts::OS,
                    std::env::consts::ARCH
                );
                Ok(())
            }
            Self::SelfCmd(command) => command.run(app).await,
        }
    }
}

pub async fn dispatch(cli: Cli, app: &AppContext) -> Result<()> {
    cli.command.run(app).await
}
