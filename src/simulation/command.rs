//! Command parsing and live-session orchestration for simulation.

use crate::AppContext;
use anyhow::Context;
use anyhow::Result;
use clap::Args;
use clap::Subcommand;
use std::path::PathBuf;

/// The `simulation` command group.
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

impl Simulation {
    pub async fn run(&self, app: &AppContext) -> Result<()> {
        match &self.command {
            SimulationSubcommand::Webots(webots) => match &webots.command {
                WebotsSubcommand::Run(command) => command.run(app).await,
            },
        }
    }
}

#[derive(Debug, Args)]
pub struct SimulationRun {
    #[arg(
        value_name = "WORLD",
        help = "Absolute path, project-relative path, or bare configured world name."
    )]
    pub world: String,
    #[arg(
        long,
        value_name = "PROJECT",
        help = "Source project directory or robot.yaml. Defaults to upward discovery."
    )]
    pub project: Option<PathBuf>,
    #[arg(
        short = 'd',
        long,
        help = "Start resident supervision and return after required startup readiness."
    )]
    pub detach: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct SimulateOptions {
    pub world: String,
    pub offline: bool,
}

impl SimulationRun {
    pub async fn run(&self, app: &AppContext) -> Result<()> {
        let target =
            crate::commands::resident::resolve_target(self.project.as_deref(), app.project.root())?;
        phoxal_cli_core::project::train::resolve_locked_train(&target.project, app.offline)
            .with_context(|| {
                format!(
                    "simulation requires a buildable source project; {} is not a source project",
                    target.project.display()
                )
            })?;
        // SAFETY: command dispatch has not started worker threads for this run.
        unsafe {
            std::env::set_var(phoxal_cli_project::PROJECT_ROOT_ENV, &target.project);
        }
        let options = SimulateOptions {
            world: self.world.clone(),
            offline: app.offline,
        };
        let resident_in_process = phoxal_cli_supervisor::resident::has_private_bootstrap()
            || (!app.output.interactive && !self.detach);
        if resident_in_process {
            return crate::run::run_webots_resident_supervision(app, target.project, options).await;
        }
        let (mut launched, client) =
            crate::run::connect_to_detached_resident(&target.project).await?;
        if self.detach {
            return crate::run::wait_for_required_readiness(&client, &mut launched.child).await;
        }
        let result = crate::commands::resident::drive_tui(app, &target, client, true).await;
        if matches!(
            result,
            Ok(crate::session::controller::AttachmentOutcome::Terminal)
        ) {
            let status = tokio::task::spawn_blocking(move || launched.child.wait()).await??;
            anyhow::ensure!(status.success(), "resident supervisor exited with {status}");
        }
        result.map(|_| ())
    }
}
