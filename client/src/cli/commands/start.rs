//! `phoxal start` - create a fresh execution, wait for readiness, and exit.
//!
//! `start` runs the same build/publish/launch path as `run`; it differs only
//! in what it does once the daemon answers. It never mounts the TUI, and it
//! never becomes the supervisor: `phoxald` is a separate executable and the
//! systemd unit starts it directly, so readiness and the watchdog belong to
//! the daemon, not to this command.

use std::path::PathBuf;

use anyhow::Result;
use clap::Args;

use crate::cli::context::AppContext;

#[derive(Debug, Args)]
pub struct Start {
    #[arg(value_name = "ROOT_OR_ENTRY")]
    pub target: Option<PathBuf>,
    #[command(flatten)]
    drivers: super::run::DriverSelection,
}

impl Start {
    pub async fn run(&self, app: &AppContext) -> Result<()> {
        let options = self.drivers.to_options()?;
        crate::application::lifecycle::start_command(app, self.target.as_deref(), options).await
    }
}

#[cfg(test)]
mod tests {
    use crate::cli::args::Cli;
    use clap::Parser;

    /// `start` shares `run`'s finalization inputs: a headless bench launch
    /// must be able to exclude hardware drivers exactly like an attached one.
    #[test]
    fn start_takes_the_same_driver_selection_as_run() {
        assert!(Cli::try_parse_from(["phoxal", "start", "--drivers", "off"]).is_ok());
        assert!(Cli::try_parse_from(["phoxal", "start", "--driver", "base"]).is_ok());
    }
}
