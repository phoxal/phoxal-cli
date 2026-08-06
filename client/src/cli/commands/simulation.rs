//! Clap-facing entry point for Webots simulation sessions.

use std::path::PathBuf;

use anyhow::Result;
use clap::{Args, Subcommand};

use crate::cli::AppContext;

#[derive(Debug, Args)]
pub struct Simulation {
    #[command(subcommand)]
    pub command: SimulationSubcommand,
}

#[derive(Debug, Subcommand)]
pub enum SimulationSubcommand {
    #[command(about = "Webots simulation commands.")]
    Webots(SimulationWebots),
}

#[derive(Debug, Args)]
pub struct SimulationWebots {
    #[command(subcommand)]
    pub command: WebotsSubcommand,
}

#[derive(Debug, Subcommand)]
pub enum WebotsSubcommand {
    #[command(about = "Run a project in Webots.")]
    Run(SimulationRun),
}

#[derive(Debug, Args)]
pub struct SimulationRun {
    #[arg(
        value_name = "WORLD",
        help = "Absolute path, project-relative path, or bare configured world name."
    )]
    pub(crate) world: String,
    #[arg(
        long,
        value_name = "PROJECT",
        help = "Source project directory or robot.yaml. Defaults to upward discovery."
    )]
    pub(crate) project: Option<PathBuf>,
}

impl Simulation {
    pub async fn run(&self, app: &AppContext) -> Result<()> {
        match &self.command {
            SimulationSubcommand::Webots(webots) => match &webots.command {
                WebotsSubcommand::Run(command) => {
                    crate::application::simulation::run_command(
                        app,
                        command.world.clone(),
                        command.project.as_deref(),
                    )
                    .await
                }
            },
        }
    }
}
