use anyhow::Result;
use async_trait::async_trait;
use clap::Args;

use phoxal_cli_core::AppContext;
use phoxal_cli_core::Command;
use phoxal_cli_core::unit::Unit;

#[derive(Debug, Args)]
pub(crate) struct Bundle {
    #[arg(help = "Robot model directory name under models/<robot-model>/")]
    pub(crate) robot_model: String,
}

#[async_trait(?Send)]
impl Command for Bundle {
    async fn execute(&self, app: &AppContext) -> Result<()> {
        phoxal_cli_core::unit::bundle::Bundle::new(self.robot_model.clone()).run(app)?;
        Ok(())
    }
}
