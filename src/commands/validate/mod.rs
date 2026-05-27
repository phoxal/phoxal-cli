use anyhow::Result;
use async_trait::async_trait;
use clap::Subcommand;

use phoxal_cli_core::AppContext;
use phoxal_cli_core::Command;

pub(crate) mod component;
pub(crate) mod robot;
pub(crate) mod scenario;

#[derive(Debug, Subcommand)]
pub(crate) enum Validate {
    #[command(about = "Validate a robot model from source files.")]
    Robot(robot::Robot),
    #[command(about = "Validate a robot component from source files.")]
    Component(component::Component),
    #[command(about = "Run framework-conformance scenario validation.")]
    Scenario(scenario::Scenario),
}

#[async_trait(?Send)]
impl Command for Validate {
    async fn execute(&self, app: &AppContext) -> Result<()> {
        match self {
            Self::Robot(command) => command.run(app).await,
            Self::Component(command) => command.run(app).await,
            Self::Scenario(command) => command.run(app).await,
        }
    }
}
