use anyhow::Result;
use async_trait::async_trait;
use clap::Subcommand;

use phoxal_cli_core::AppContext;
use phoxal_cli_core::Command;

pub(crate) mod fleet;
pub(crate) mod local;

#[derive(Debug, Subcommand)]
pub(crate) enum Deploy {
    #[command(about = "Build and deploy a robot model to a local balenaOS device.")]
    Local(local::Local),
    #[command(about = "Build and deploy a robot model to a remote balena fleet.")]
    Fleet(fleet::Fleet),
}

#[async_trait(?Send)]
impl Command for Deploy {
    async fn execute(&self, app: &AppContext) -> Result<()> {
        match self {
            Self::Local(command) => command.run(app).await,
            Self::Fleet(command) => command.run(app).await,
        }
    }
}
