//! Install an immutable deployment release: its bundle and the daemon that
//! runs it, activated together.

use std::path::PathBuf;

use anyhow::Result;
use clap::Args;

#[derive(Debug, Args)]
pub struct Install {
    #[arg(value_name = "BUILD_PHOXAL")]
    archive: PathBuf,
}

impl Install {
    pub async fn run(&self, app: &crate::cli::context::AppContext) -> Result<()> {
        crate::application::installation::install_command(app, &self.archive).await
    }
}
