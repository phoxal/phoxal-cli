//! Self-upgrade command adapters.

use anyhow::Result;
use clap::{Args, Subcommand};
use semver::Version;

use crate::cli::AppContext;

#[derive(Debug, Args)]
pub struct SelfCmd {
    #[command(subcommand)]
    pub command: SelfSubcommand,
}

#[derive(Debug, Subcommand)]
pub enum SelfSubcommand {
    #[command(about = "Upgrade this phoxal executable.")]
    Upgrade(Upgrade),
}

#[derive(Debug, Args)]
pub struct Upgrade {
    #[arg(
        long,
        value_name = "tag",
        value_parser = parse_version_arg,
        help = "Install this release tag (e.g. v0.5.0) instead of the latest."
    )]
    pub version: Option<Version>,
    #[arg(long, help = "Reinstall even when already on the target version.")]
    pub force: bool,
}

impl SelfCmd {
    pub async fn run(&self, app: &AppContext) -> Result<()> {
        match &self.command {
            SelfSubcommand::Upgrade(command) => {
                crate::bootstrap::upgrade(app, command.version.clone(), command.force).await
            }
        }
    }
}

pub fn parse_version_arg(raw: &str) -> std::result::Result<Version, String> {
    crate::bootstrap::parse_version_arg(raw)
}
