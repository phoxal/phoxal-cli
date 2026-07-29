//! Clap-facing entry point for local robot sessions.

use std::path::PathBuf;

use anyhow::{Result, bail};
use clap::{Args, ValueEnum};

use crate::cli::AppContext;

#[derive(Debug, Args)]
pub struct Run {
    #[arg(value_name = "PROJECT")]
    target: Option<PathBuf>,
    #[arg(
        short = 'd',
        long,
        help = "Start resident supervision and return after required startup readiness."
    )]
    pub(crate) detach: bool,
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

impl Run {
    pub async fn run(&self, app: &AppContext) -> Result<()> {
        if self.drivers == DriversMode::Off && !self.drivers_subset.is_empty() {
            bail!("--driver cannot be combined with --drivers off");
        }
        crate::application::run::run_command(
            app,
            self.target.as_deref(),
            self.detach,
            match self.drivers {
                DriversMode::On => crate::application::run::DriversMode::On,
                DriversMode::Off => crate::application::run::DriversMode::Off,
            },
            self.drivers_subset.clone(),
        )
        .await
    }
}
