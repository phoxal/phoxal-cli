//! `phoxal attach` - join an existing execution. It never builds.

use std::path::PathBuf;

use anyhow::Result;
use clap::Args;

use crate::cli::context::AppContext;

#[derive(Debug, Args)]
pub struct Attach {
    #[arg(value_name = "PROJECT_OR_ENTRY")]
    target: Option<PathBuf>,
    #[arg(
        long,
        value_name = "ZENOH_ENDPOINT",
        help = "Attach at an explicit endpoint instead of the one this project resolves to."
    )]
    endpoint: Option<String>,
}

impl Attach {
    pub async fn run(&self, app: &AppContext) -> Result<()> {
        crate::application::lifecycle::attach_command(
            app,
            self.target.as_deref(),
            self.endpoint.clone(),
        )
        .await
    }
}
