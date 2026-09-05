//! Clap-facing entry point for local world simulation sessions.

use std::path::PathBuf;

use anyhow::Result;
use clap::{Args, Subcommand};

use crate::cli::context::AppContext;

#[derive(Debug, Args)]
pub struct Simulation {
    #[command(subcommand)]
    pub command: SimulationSubcommand,
}

#[derive(Debug, Subcommand)]
pub enum SimulationSubcommand {
    #[command(about = "Start a world, connect this robot, and attach to the robot.")]
    Run(SimulationRun),
    #[command(about = "Start a world headlessly and leave it paused.")]
    Start(SimulationStart),
    #[command(about = "Open the terminal view for a live world instance.")]
    Open(InstanceCommand),
    #[command(about = "Connect a fresh driver-free execution to a live world.")]
    Connect(SimulationConnect),
    #[command(about = "Report live state or retained terminal evidence for a world.")]
    Status(InstanceCommand),
    #[command(about = "Print retained process evidence for a world.")]
    Logs(InstanceCommand),
    #[command(about = "List live world instances.")]
    List(SimulationList),
    #[command(about = "Stop a live world and every robot attached to it.")]
    Stop(InstanceCommand),
}

#[derive(Debug, Args)]
pub struct SimulationRun {
    #[arg(value_name = "WORLD", help = "Explicit path to world.yaml.")]
    pub(crate) world: PathBuf,
    #[arg(long, value_name = "NAME", help = "Named world spawn point.")]
    pub(crate) spawn: Option<String>,
}

#[derive(Debug, Args)]
pub struct SimulationStart {
    #[arg(value_name = "WORLD", help = "Explicit path to world.yaml.")]
    pub(crate) world: PathBuf,
}

#[derive(Debug, Args)]
pub struct SimulationConnect {
    #[arg(value_name = "INSTANCE_ID", help = "Complete world instance ID.")]
    pub(crate) instance: String,
    #[arg(long, value_name = "NAME", help = "Named world spawn point.")]
    pub(crate) spawn: Option<String>,
}

#[derive(Debug, Args)]
pub struct InstanceCommand {
    #[arg(value_name = "INSTANCE_ID", help = "Complete world instance ID.")]
    pub(crate) instance: String,
}

#[derive(Debug, Args)]
pub struct SimulationList {
    #[arg(long, help = "Include retained terminal world sessions.")]
    pub(crate) all: bool,
}

impl Simulation {
    #[must_use]
    pub(crate) const fn requires_terminal(&self) -> bool {
        matches!(
            self.command,
            SimulationSubcommand::Run(_)
                | SimulationSubcommand::Open(_)
                | SimulationSubcommand::Connect(_)
        )
    }

    pub async fn run(&self, app: &AppContext) -> Result<()> {
        match &self.command {
            SimulationSubcommand::Run(command) => {
                crate::application::simulation::run_command(
                    app,
                    &command.world,
                    command.spawn.as_deref(),
                )
                .await
            }
            SimulationSubcommand::Start(command) => {
                crate::application::simulation::start_command(app, &command.world).await
            }
            SimulationSubcommand::Open(command) => {
                crate::application::simulation::open_command(app, &command.instance).await
            }
            SimulationSubcommand::Connect(command) => {
                crate::application::simulation::connect_command(
                    app,
                    &command.instance,
                    command.spawn.as_deref(),
                )
                .await
            }
            SimulationSubcommand::Status(command) => {
                crate::application::simulation::status_command(app, &command.instance).await
            }
            SimulationSubcommand::Logs(command) => {
                crate::application::simulation::logs_command(app, &command.instance).await
            }
            SimulationSubcommand::List(command) => {
                crate::application::simulation::list_command(app, command.all).await
            }
            SimulationSubcommand::Stop(command) => {
                crate::application::simulation::stop_command(app, &command.instance).await
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use crate::cli::args::{Cli, RootCommand};

    use super::SimulationSubcommand;

    fn simulation(args: &[&str]) -> SimulationSubcommand {
        let cli = Cli::try_parse_from(args).expect("command parses");
        let RootCommand::Simulation(simulation) = cli.command else {
            panic!("expected simulation command");
        };
        simulation.command
    }

    #[test]
    fn parses_the_complete_backend_neutral_surface() {
        assert!(matches!(
            simulation(&["phoxal", "simulation", "run", "world.yaml"]),
            SimulationSubcommand::Run(_)
        ));
        assert!(matches!(
            simulation(&["phoxal", "simulation", "start", "world.yaml"]),
            SimulationSubcommand::Start(_)
        ));
        assert!(matches!(
            simulation(&[
                "phoxal",
                "simulation",
                "open",
                "0123456789abcdef0123456789abcdef"
            ]),
            SimulationSubcommand::Open(_)
        ));
        assert!(matches!(
            simulation(&[
                "phoxal",
                "simulation",
                "connect",
                "0123456789abcdef0123456789abcdef",
                "--spawn",
                "loading-bay",
            ]),
            SimulationSubcommand::Connect(_)
        ));
        assert!(matches!(
            simulation(&[
                "phoxal",
                "simulation",
                "status",
                "0123456789abcdef0123456789abcdef"
            ]),
            SimulationSubcommand::Status(_)
        ));
        assert!(matches!(
            simulation(&[
                "phoxal",
                "simulation",
                "logs",
                "0123456789abcdef0123456789abcdef"
            ]),
            SimulationSubcommand::Logs(_)
        ));
        assert!(matches!(
            simulation(&["phoxal", "simulation", "list", "--all"]),
            SimulationSubcommand::List(_)
        ));
        assert!(matches!(
            simulation(&[
                "phoxal",
                "simulation",
                "stop",
                "0123456789abcdef0123456789abcdef"
            ]),
            SimulationSubcommand::Stop(_)
        ));
    }

    #[test]
    fn rejects_retired_backend_and_control_options() {
        for args in [
            vec!["phoxal", "simulation", "webots", "run", "world.yaml"],
            vec![
                "phoxal",
                "simulation",
                "start",
                "world.yaml",
                "--mode",
                "live",
            ],
            vec![
                "phoxal",
                "simulation",
                "start",
                "world.yaml",
                "--profile",
                "live",
            ],
            vec!["phoxal", "simulation", "start", "world.yaml", "--sync"],
            vec![
                "phoxal",
                "simulation",
                "start",
                "world.yaml",
                "--speed",
                "1.0",
            ],
            vec![
                "phoxal",
                "simulation",
                "connect",
                "0123456789abcdef0123456789abcdef",
                "--detach",
            ],
        ] {
            assert!(Cli::try_parse_from(args).is_err());
        }
    }
}
