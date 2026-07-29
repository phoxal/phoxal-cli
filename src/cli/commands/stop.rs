//! Stop a resident runtime.

use std::path::PathBuf;

use anyhow::Result;
use clap::Args;

use crate::cli::AppContext;

#[derive(Debug, Args)]
pub struct Stop {
    #[arg(value_name = "PROJECT_OR_ENTRY")]
    target: Option<PathBuf>,
    #[arg(long)]
    force: bool,
}

impl Stop {
    pub async fn run(&self, app: &AppContext) -> Result<()> {
        crate::application::attachment::stop_command(app, self.target.as_deref(), self.force).await
    }
}
