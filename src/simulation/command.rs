//! Command parsing and live-session orchestration for simulation.

use super::{LiveSimSetup, live_simulate_setup, prepare, prepare_with_mode, report_plan_only};
use crate::AppContext;
use anyhow::Context;
use anyhow::Result;
use clap::Args;
use clap::Subcommand;
use phoxal_cli_core::project::catalog::Catalog;
use phoxal_cli_core::project::launch_plan::LaunchPlan;
use phoxal_cli_core::project::launch_plan::PlanContext;
use phoxal_cli_core::project::resolver::ResolvedRobot;
use phoxal_cli_core::project::resolver::RobotManifestExtras;
use std::path::PathBuf;

/// The `simulation` command group: `run` (this repo's original `simulate
/// <world>` verb, renamed) and `join` (a multi-robot join stub - see
/// [`SimulationJoin`]). Deliberately a clean cut, no `simulate` alias and no
/// bare `simulation <world>` shorthand - see the group's module docs.
#[derive(Debug, Args)]
pub struct Simulation {
    #[command(subcommand)]
    pub command: SimulationSubcommand,
}

#[derive(Debug, Subcommand)]
pub enum SimulationSubcommand {
    #[command(about = "Resolve and report a Webots simulation launch plan.")]
    Run(SimulationRun),
    #[command(about = "Join a running multi-robot simulation session (not available yet).")]
    Join(SimulationJoin),
}

impl Simulation {
    pub async fn run(&self, app: &AppContext) -> Result<()> {
        match &self.command {
            SimulationSubcommand::Run(command) => command.run(app).await,
            SimulationSubcommand::Join(command) => command.run(app).await,
        }
    }
}

#[derive(Debug, Args)]
pub struct SimulationRun {
    #[arg(
        value_name = "WORLD",
        help = "World file or bare name (e.g. `default`, or `worlds/foo.wbt`). Resolved against <project>/worlds/<world>.wbt, then <project>/<world>."
    )]
    pub world: String,
    #[arg(
        long,
        help = "Resolve and write run artifacts without starting simulation processes."
    )]
    pub dry_run: bool,
    #[arg(
        long,
        help = "Watch local source artifacts and hot-reload checked changes."
    )]
    pub watch: bool,
    #[arg(
        long = "env",
        value_name = "ENV",
        help = "Apply a robot.<env>.yaml overlay before simulating (repeatable). Path pins are only legal through overlays."
    )]
    pub env: Vec<String>,
    #[arg(
        long,
        value_name = "TRIPLE",
        help = "Resolve the robot's official artifacts for this target instead of the host (e.g. aarch64, x86_64, or a full triple). The simulator itself still runs on the host. Use it to plan a Linux robot's simulation from a non-Linux host."
    )]
    pub target: Option<String>,
}

/// `phoxal-cli simulation join`: joins a running multi-robot simulation
/// session. Multi-robot join lands as its own slice - this is a stub that
/// prints a clear "not available yet" message and exits 0 (it is not an
/// error to ask for a feature that is on the roadmap but not yet wired up;
/// scripts should not need to special-case this verb's exit status).
#[derive(Debug, Args)]
pub struct SimulationJoin {}

impl SimulationJoin {
    pub async fn run(&self, app: &AppContext) -> Result<()> {
        app.ui.info(
            "simulation join: not available yet - multi-robot join lands in a separate slice",
        );
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SimulateMode {
    Live,
    DryRun,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SimulateOptions {
    pub world: String,
    pub catalog_source: Option<String>,
    pub watch: bool,
    pub overlays: Vec<String>,
    pub target: Option<String>,
}

/// Pairs the sim `LaunchPlan` with its `PlanContext` (Part 3/6): replaces the
/// old `SimulatePlan` wrapper, which re-declared `resolved`/`project_root`/
/// `source_participants`/`robot_path` fields `PlanContext` now owns, plus a
/// sim-only `world_path` now carried directly by `LaunchMode::Webots` and a
/// `bus_connect` that was always just `DEFAULT_ROUTER_CONNECT`, and a
/// `native_tools` display list now computed from the plan at print time (see
/// `native_tool_labels_from_plan`).
#[derive(Debug, Clone, PartialEq)]
pub struct SimPlan {
    pub plan: LaunchPlan,
    pub ctx: PlanContext,
    /// Finding A5: this session's launch-time participant metadata, resolved
    /// once in `prepare_with_mode` from `plan` and its (sim-filtered)
    /// contract surfaces - see `phoxal_cli_core::session::stores::runtime::RuntimeStore`'s
    /// own docs.
    pub runtime_store: phoxal_cli_core::session::stores::runtime::RuntimeStore,
}

pub(crate) struct ResolvedSimulation {
    pub(crate) robot_path: PathBuf,
    pub(crate) project_root: PathBuf,
    pub(crate) world_path: PathBuf,
    pub(crate) resolved: ResolvedRobot,
    pub(crate) manifest_extras: RobotManifestExtras,
    pub(crate) catalog: Option<Catalog>,
}

impl SimulationRun {
    pub async fn run(&self, app: &AppContext) -> Result<()> {
        let options = SimulateOptions {
            world: self.world.clone(),
            catalog_source: app.catalog_source.clone(),
            watch: self.watch,
            overlays: self.env.clone(),
            target: self.target.clone(),
        };
        let mode = if self.dry_run {
            SimulateMode::DryRun
        } else {
            SimulateMode::Live
        };
        run(app, options, mode).await.map(|_| ())
    }
}

pub async fn run(
    app: &AppContext,
    options: SimulateOptions,
    mode: SimulateMode,
) -> Result<SimPlan> {
    match mode {
        SimulateMode::DryRun => {
            let project_root = app.project.root().to_path_buf();
            let sim = tokio::task::spawn_blocking(move || prepare(&project_root, options))
                .await
                .context("simulate dry-run worker failed")??;
            report_plan_only(&sim)?;
            Ok(sim)
        }
        SimulateMode::Live => {
            let identity = crate::supervisor::SupervisorIdentity::resolve(
                app.project.root(),
                phoxal_cli_core::session::SessionMode::Simulation,
            )?;
            let _supervisor_lock = crate::supervisor::SupervisorLock::acquire(identity)?;
            // One interactive surface for the whole session (Product
            // decision 1): the controller starts its renderer right now,
            // before preparation even begins - see `SessionController::new`.
            let mut controller = crate::session::controller::SessionController::new(
                app.output,
                phoxal_cli_core::session::SessionMode::Simulation,
                app.project.root(),
            )?;
            let events = controller.events();

            let project_root = app.project.root().to_path_buf();
            let ui = app.ui;
            let prepared_options = options.clone();
            let sim = controller
                .drive_prepare_phase(move || {
                    prepare_with_mode(&project_root, prepared_options, SimulateMode::Live)
                })
                .await?;

            // Fixes finding A1: preflight, lock acquisition, world/controller
            // staging, and spawn-responder startup used to run as a
            // synchronous gap AFTER preparation but BEFORE the controller's
            // own Ctrl-C-aware loop resumed - in a raw-mode TUI, Ctrl-C is a
            // KEY EVENT (crossterm disables ISIG), which was simply never
            // polled during that whole window. `drive_setup` keeps ONE
            // controller loop live (input, Ctrl-C, session events, redraws)
            // through this setup phase too, exactly like
            // `drive_prepare_phase` already does for preparation.
            let token = controller.token();
            let output = controller.output();
            let renders_tui = controller.renders_tui();
            let setup = live_simulate_setup(
                ui,
                sim.clone(),
                options.clone(),
                events,
                token,
                output,
                renders_tui,
            );
            let setup = controller.drive_setup(setup).await?;

            let LiveSimSetup {
                router,
                connect,
                _locks: locks,
                board,
                telemetry,
                runtime_store,
                orderly_shutdown_timeout,
                stages,
                supervisor_options,
                action_tx,
                background_tasks,
            } = setup;
            controller.set_bus_endpoint(connect);
            controller.set_restart_channel(action_tx);
            let supervise_task =
                tokio::spawn(router.supervise(stages, board.clone(), supervisor_options));

            let outcome = controller
                .drive_supervision(
                    board,
                    telemetry,
                    runtime_store,
                    orderly_shutdown_timeout,
                    supervise_task,
                )
                .await;
            drop(background_tasks);
            drop(locks);
            let outcome = outcome?;
            // Participant failures were already rendered continuously on the
            // board. `drive_supervision` has consumed and torn down the
            // controller, so retain a non-fatal plain-mode summary without
            // converting them into a command error.
            if !outcome.failed_participants.is_empty() {
                app.ui.warn(format!(
                    "simulation stopped with failed participants: {}",
                    outcome.failed_participants.join(", ")
                ));
            }
            Ok(sim)
        }
    }
}
