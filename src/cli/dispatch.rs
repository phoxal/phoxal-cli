//! Command dispatch boundary.

use std::io::IsTerminal;

use anyhow::{Result, bail};

use super::args::{Cli, RootCommand};
use super::commands::simulation;
use super::{AppContext, Ui};

impl RootCommand {
    fn enters_interactive_session(&self) -> bool {
        match self {
            Self::Run(run) => !run.detach,
            Self::Attach(_) => true,
            Self::Simulation(command) => match &command.command {
                simulation::SimulationSubcommand::Webots(webots) => match &webots.command {
                    simulation::WebotsSubcommand::Run(run) => !run.detach,
                },
            },
            _ => false,
        }
    }

    async fn run(&self, app: &AppContext) -> Result<()> {
        match self {
            Self::Build(command) => command.run(app).await,
            Self::Deploy(command) => command.run(app).await,
            Self::Install(command) => command.run(app).await,
            Self::Rollback(command) => command.run(app).await,
            Self::Validate(command) => command.run(app).await,
            Self::Simulation(command) => command.run(app).await,
            Self::Run(command) => command.run(app).await,
            Self::Start(command) => command.run(app).await,
            Self::Attach(command) => command.run(app).await,
            Self::Stop(command) => command.run(app).await,
            Self::Logs(command) => command.run(app).await,
            Self::Status(command) => command.run(app).await,
            Self::Doctor(command) => command.run(app).await,
            Self::Service(command) => command.run(app).await,
            Self::Version(command) => command.run(),
            Self::SelfCmd(command) => command.run(app).await,
        }
    }
}

pub async fn dispatch(cli: Cli, app: &AppContext) -> Result<()> {
    let terminal = std::io::stderr().is_terminal();
    if cli.command.enters_interactive_session()
        && !terminal
        && !matches!(cli.command, RootCommand::Run(_))
    {
        bail!(
            "interactive `run` and `simulation webots run` sessions require a terminal; run this command in a TTY"
        );
    }
    let output = crate::cli::output::OutputContext::compute(terminal);
    let app = &AppContext {
        output,
        ui: Ui::new(output.decorated()),
        ..app.clone()
    };
    cli.command.run(app).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn enters_interactive_session_covers_run_and_live_simulate_only() {
        let run = Cli::try_parse_from(["phoxal", "run"]).unwrap();
        assert!(run.command.enters_interactive_session());

        let live =
            Cli::try_parse_from(["phoxal", "simulation", "webots", "run", "default"]).unwrap();
        assert!(live.command.enters_interactive_session());

        let detached = Cli::try_parse_from([
            "phoxal",
            "simulation",
            "webots",
            "run",
            "default",
            "--detach",
        ])
        .unwrap();
        assert!(!detached.command.enters_interactive_session());

        let start = Cli::try_parse_from(["phoxal", "start"]).unwrap();
        assert!(!start.command.enters_interactive_session());
    }
}
