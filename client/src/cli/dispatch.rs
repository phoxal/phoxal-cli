//! Command dispatch boundary.

use std::io::IsTerminal;

use anyhow::{Result, bail};

use super::args::{Cli, RootCommand};
use super::{AppContext, Ui};

impl RootCommand {
    /// Whether this invocation is a pure attachment client with no headless
    /// fallback. `run` and foreground `simulation webots run` degrade to an
    /// in-process headless resident when stderr is not a terminal (see
    /// `application::run::should_run_resident_in_process`); `attach` has
    /// nothing to drive but the TUI, so it alone requires a TTY.
    fn requires_terminal(&self) -> bool {
        matches!(self, Self::Attach(_))
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
        bail!("interactive `attach` sessions require a terminal; run this command in a TTY");
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

    #[test]
    fn only_attach_requires_a_terminal() {
        let attach = Cli::try_parse_from(["phoxal", "attach"]).unwrap();
        assert!(attach.command.requires_terminal());

        // `run` and foreground `simulation webots run` fall back to a headless
        // in-process resident without a TTY - the resident child re-executes
        // them with stderr bound to the supervisor log.
        let run = Cli::try_parse_from(["phoxal", "run"]).unwrap();
        assert!(!run.command.requires_terminal());

        let live =
            Cli::try_parse_from(["phoxal", "simulation", "webots", "run", "default"]).unwrap();
        assert!(!live.command.requires_terminal());

        let start = Cli::try_parse_from(["phoxal", "start"]).unwrap();
        assert!(!start.command.requires_terminal());
    }
}
