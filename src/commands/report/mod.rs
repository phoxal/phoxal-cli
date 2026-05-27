use anyhow::Result;
use async_trait::async_trait;
use clap::Subcommand;

use phoxal_cli_core::AppContext;
use phoxal_cli_core::Command;

pub(crate) mod conformance;
pub(crate) mod inventory;
pub(crate) mod roles;

#[derive(Debug, Subcommand)]
pub(crate) enum Report {
    #[command(about = "Print autonomy profile conformance.")]
    Conformance(conformance::Conformance),
    #[command(about = "Print resolved capability roles.")]
    Roles(roles::Roles),
    #[command(about = "Print owner-local topic and query inventory.")]
    Inventory(inventory::Inventory),
}

#[async_trait(?Send)]
impl Command for Report {
    async fn execute(&self, app: &AppContext) -> Result<()> {
        match self {
            Self::Conformance(command) => command.run(app).await,
            Self::Roles(command) => command.run(app).await,
            Self::Inventory(command) => command.run(app).await,
        }
    }
}
