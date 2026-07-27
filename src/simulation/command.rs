//! Command parsing and live-session orchestration for simulation.

use crate::AppContext;
use anyhow::Context;
use anyhow::Result;
use clap::Args;
use clap::Subcommand;
use phoxal_cli_core::project::launch_plan::LaunchPlan;
use phoxal_cli_core::project::launch_plan::PlanContext;
use phoxal_cli_core::project::resolver::ResolvedRobot;
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

/// Pairs the sim `LaunchPlan` with its `PlanContext` (Part 3/6): replaces the
/// old `SimulatePlan` wrapper, which re-declared `resolved`/`project_root`/
/// `source_participants`/`robot_path` fields `PlanContext` now owns, plus a
/// sim-only `world_path` now carried directly by `LaunchMode::Webots` and a
/// `bus_connect` that was always just `DEFAULT_ROUTER_CONNECT`, and a
/// `native_tools` display list now computed from the plan at print time (see
/// `native_tool_labels_from_plan`).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SimPlan {
    pub plan: LaunchPlan,
    pub ctx: PlanContext,
}

/// Simulation always prepares from a source project, so its `PlanContext`
/// always carries the source graph; this is the one checked unwrap (#936).
pub(crate) fn sim_source(sim: &SimPlan) -> &phoxal_cli_core::project::launch_plan::PlanSource {
    sim.ctx
        .source
        .as_ref()
        .expect("simulation always prepares from a source project")
}

pub(crate) struct ResolvedSimulation {
    pub(crate) robot_path: PathBuf,
    pub(crate) project_root: PathBuf,
    pub(crate) world_path: PathBuf,
    pub(crate) resolved: ResolvedRobot,
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
            std::env::set_var(crate::host_paths::PROJECT_ROOT_ENV, &target.project);
        }
        let options = SimulateOptions {
            world: self.world.clone(),
            offline: app.offline,
        };
        let resident_in_process =
            crate::resident::has_private_bootstrap() || (!app.output.interactive && !self.detach);
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
