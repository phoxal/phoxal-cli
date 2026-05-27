use std::path::PathBuf;

use anyhow::Result;
use async_trait::async_trait;
use clap::Args;

use crate::commands::{Command, ContainerSelectionArgs, GenerateComposeMode};
use phoxal_cli_core::AppContext;

#[derive(Debug, Args)]
pub(crate) struct Compose {
    #[arg(help = "Robot model directory name under models/<robot-model>/")]
    pub(crate) robot_model: String,
    #[arg(help = "Generation mode")]
    pub(crate) mode: GenerateComposeMode,
    #[command(flatten)]
    pub(crate) container_selection: ContainerSelectionArgs,
    #[arg(long = "output")]
    pub(crate) output_path: Option<PathBuf>,
}

#[async_trait(?Send)]
impl Command for Compose {
    async fn execute(&self, app: &AppContext) -> Result<()> {
        super::generate_bundle_and_compose(
            app,
            &self.robot_model,
            self.mode,
            self.container_selection.clone(),
            self.output_path.clone(),
        )
    }
}
