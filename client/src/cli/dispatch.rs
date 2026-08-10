//! Command dispatch boundary.

use std::io::IsTerminal;

use anyhow::{Result, bail};

use super::args::{Cli, RootCommand};
use super::context::AppContext;
use super::output::Ui;

impl RootCommand {
    /// Whether this invocation mounts the terminal application.
    ///
    /// A command that attaches drives the TUI; supervision lives in `phoxald`.
    /// `start` is the headless verb - it launches, waits for readiness, and
    /// returns without ever attaching.
    fn requires_terminal(&self) -> bool {
        matches!(self, Self::Attach(_) | Self::Run(_) | Self::Simulation(_))
    }

    async fn run(&self, app: &AppContext) -> Result<()> {
        match self {
            Self::Build(command) => command.run(app).await,
            Self::Deploy(command) => command.run(app).await,
            Self::Install(command) => command.run(app).await,
            Self::Rollback(command) => command.run(app).await,
            Self::Validate(command) => command.run(app).await,
            Self::Schema(command) => command.run(app).await,
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
    if cli.command.requires_terminal() && !terminal {
        bail!(
            "this command attaches a terminal session and needs a TTY. For a headless \
             launch use `phoxal start`, and to inspect an execution use `phoxal status` or \
             `phoxal logs`"
        );
    }
    let output = crate::cli::output::OutputContext::compute(terminal);
    let app = &AppContext {
        output,
        ui: Ui::new(output.decorated(), false),
        ..app.clone()
    };
    cli.command.run(app).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    /// Every attaching command needs a TTY; `start` and the project commands
    /// do not, because they never mount the terminal application.
    #[test]
    fn every_attaching_command_requires_a_terminal_and_start_does_not() {
        for attaching in [
            vec!["phoxal", "attach"],
            vec!["phoxal", "run"],
            vec!["phoxal", "simulation", "webots", "run", "default"],
        ] {
            let cli = Cli::try_parse_from(attaching.clone()).unwrap();
            assert!(
                cli.command.requires_terminal(),
                "{attaching:?} mounts the TUI and must require a terminal"
            );
        }
        for headless in [
            vec!["phoxal", "start"],
            vec!["phoxal", "status"],
            vec!["phoxal", "stop"],
            vec!["phoxal", "logs"],
            vec!["phoxal", "build"],
        ] {
            let cli = Cli::try_parse_from(headless.clone()).unwrap();
            assert!(
                !cli.command.requires_terminal(),
                "{headless:?} never mounts the TUI"
            );
        }
    }
}
