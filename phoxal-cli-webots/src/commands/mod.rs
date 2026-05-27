use anyhow::Result;
use async_trait::async_trait;
use clap::{Parser, Subcommand};

use phoxal_cli_core::AppContext;
use phoxal_cli_core::Command;

mod import;
mod process;
mod scenario;
pub mod session;
mod stage;

use import::Import;
use scenario::Scenario;
use session::{Down, Reset, Up};
use stage::Stage;

const SUPERVISOR_PACKAGE: &str = "phoxal-simulator-webots-supervisor";
const SUPERVISOR_BINARY: &str = "phoxal-simulator-webots-supervisor";
const CONTROLLER_PACKAGE: &str = "phoxal-simulator-webots-controller";
const CONTROLLER_BINARY: &str = "phoxal-simulator-webots-controller";
const RERUN_PROXY_PACKAGE: &str = "phoxal-rerun-proxy";
const JOYPAD_PACKAGE: &str = "phoxal-joypad";
pub const LOCAL_HOST_ROUTER_ENDPOINT: &str = "tcp/127.0.0.1:7447";
const INTERACTIVE_CONNECT_TIMEOUT_MS: u64 = 5_000;
const INTERACTIVE_CONNECT_RETRIES: u32 = 60;

#[derive(Debug, Parser)]
pub struct Webots {
    #[command(subcommand)]
    pub action: Action,
}

#[derive(Debug, Subcommand)]
pub enum Action {
    #[command(about = "Import an OpenStreetMap file into a Webots world source file.")]
    Import(Import),
    #[command(about = "Stage Webots artifacts without starting the simulation session.")]
    Stage(Stage),
    #[command(about = "Bring up the local simulation session.")]
    Up(Up),
    #[command(about = "Run non-interactive robot-acceptance scenarios in Webots.")]
    Scenario(Scenario),
    #[command(about = "Stop the local simulation session cleanly.")]
    Down(Down),
    #[command(about = "Provide a simple full restart path during active development.")]
    Restart(Up),
    #[command(about = "Reset only the simulation state without full session teardown.")]
    Reset(Reset),
}

#[async_trait(?Send)]
impl Command for Webots {
    async fn execute(&self, app: &AppContext) -> Result<()> {
        match &self.action {
            Action::Import(args) => args.execute(app),
            Action::Stage(args) => args.execute(app),
            Action::Up(args) => args.execute(app),
            Action::Scenario(args) => args.execute(app).await,
            Action::Down(args) => args.execute(app),
            Action::Restart(args) => {
                let down = Down {
                    robot_model: args.robot_model.clone(),
                    force: true,
                    robot_id: args.robot_id.clone(),
                    robot_namespace: args.robot_namespace.clone(),
                };
                down.execute(app)?;
                args.execute(app)
            }
            Action::Reset(args) => args.execute(app).await,
        }
    }
}
