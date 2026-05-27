use anyhow::Result;
use async_trait::async_trait;
use clap::Args;

use phoxal_cli_core::AppContext;
use phoxal_cli_core::Command;
use phoxal_cli_core::unit::robot::ValidatedRobot;

#[derive(Debug, Args)]
pub(crate) struct Robot {
    #[arg(help = "Robot model directory name under models/<robot-model>/")]
    pub(crate) robot_model: String,
}

#[async_trait(?Send)]
impl Command for Robot {
    async fn execute(&self, app: &AppContext) -> Result<()> {
        let _robot = ValidatedRobot::load(app, &self.robot_model)?;
        app.ui
            .success(format!("Robot '{}' is valid", self.robot_model));
        Ok(())
    }
}
