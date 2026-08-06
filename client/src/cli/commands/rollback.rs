//! Activate a previously installed runtime.

use anyhow::Result;
use clap::Args;

#[derive(Debug, Args)]
pub struct Rollback {
    #[arg(long, value_name = "RELEASE_DIRECTORY_NAME")]
    to: Option<String>,
}

impl Rollback {
    pub async fn run(&self, app: &crate::cli::AppContext) -> Result<()> {
        crate::application::installation::rollback_command(app, self.to.as_deref()).await
    }
}
