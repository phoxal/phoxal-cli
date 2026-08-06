//! `phoxal run` - create a fresh execution and attach to it.

use std::path::PathBuf;

use anyhow::{Result, bail};
use clap::{Args, ValueEnum};

use crate::cli::AppContext;

#[derive(Debug, Args)]
pub struct Run {
    #[arg(value_name = "PROJECT")]
    target: Option<PathBuf>,
    #[command(flatten)]
    drivers: DriverSelection,
}

/// The driver-selection finalization inputs `run` and `start` share: they
/// decide what the staged manifest contains, before any binary resolution.
#[derive(Debug, Args)]
pub(crate) struct DriverSelection {
    #[arg(
        long = "driver",
        value_name = "ID",
        help = "Launch only the named component driver. Repeat for a strict bench subset."
    )]
    drivers_subset: Vec<String>,
    #[arg(
        long = "drivers",
        value_enum,
        default_value_t = DriversMode::On,
        help = "Driver launch policy."
    )]
    drivers: DriversMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, ValueEnum)]
enum DriversMode {
    #[default]
    On,
    Off,
}

impl DriverSelection {
    pub(crate) fn to_options(&self) -> Result<crate::application::lifecycle::RunOptions> {
        if self.drivers == DriversMode::Off && !self.drivers_subset.is_empty() {
            bail!("--driver cannot be combined with --drivers off");
        }
        Ok(crate::application::lifecycle::RunOptions {
            drivers: match self.drivers {
                DriversMode::On => crate::application::lifecycle::DriversMode::On,
                DriversMode::Off => crate::application::lifecycle::DriversMode::Off,
            },
            drivers_subset: self.drivers_subset.clone(),
        })
    }
}

impl Run {
    pub async fn run(&self, app: &AppContext) -> Result<()> {
        let options = self.drivers.to_options()?;
        crate::application::lifecycle::run_command(app, self.target.as_deref(), options).await
    }
}

#[cfg(test)]
mod tests {
    use crate::cli::args::{Cli, RootCommand};
    use clap::Parser;

    /// `run` always creates a fresh execution, so there is no detach flag: an
    /// operator who wants a launched execution and their terminal back runs
    /// `start` (organization#978).
    #[test]
    fn run_takes_driver_selection_and_no_detach_flag() {
        let cli = Cli::try_parse_from(["phoxal", "run", "--drivers", "off"]).expect("run parses");
        assert!(matches!(cli.command, RootCommand::Run(_)));
        assert!(Cli::try_parse_from(["phoxal", "run", "-d"]).is_err());
        assert!(Cli::try_parse_from(["phoxal", "run", "--detach"]).is_err());
        // A remote endpoint on `run` would mean building here and executing
        // there, which this command cannot do.
        assert!(Cli::try_parse_from(["phoxal", "run", "--endpoint", "tcp/robot:7447"]).is_err());
    }

    #[test]
    fn a_driver_subset_contradicts_drivers_off() {
        let cli = Cli::try_parse_from(["phoxal", "run", "--drivers", "off", "--driver", "base"])
            .expect("the combination parses and is rejected at run time");
        assert!(matches!(cli.command, RootCommand::Run(_)));
    }
}
