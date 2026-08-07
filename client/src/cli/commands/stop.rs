//! `phoxal stop` - end an existing execution.

use std::path::PathBuf;

use anyhow::Result;
use clap::Args;

use crate::cli::AppContext;

#[derive(Debug, Args)]
pub struct Stop {
    #[arg(value_name = "PROJECT_OR_ENTRY")]
    target: Option<PathBuf>,
    #[arg(
        long,
        value_name = "ZENOH_ENDPOINT",
        help = "Stop the execution at an explicit endpoint instead of this project's."
    )]
    endpoint: Option<String>,
}

impl Stop {
    pub async fn run(&self, app: &AppContext) -> Result<()> {
        crate::application::lifecycle::stop_command(
            app,
            self.target.as_deref(),
            self.endpoint.clone(),
        )
        .await
    }
}
