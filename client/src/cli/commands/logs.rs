//! `phoxal logs` - read the supervisor's retained participant logs.

use std::path::PathBuf;

use anyhow::Result;
use clap::Args;

use crate::cli::context::AppContext;

#[derive(Debug, Args)]
pub struct Logs {
    #[arg(short = 'f', long, help = "Follow log records until interrupted.")]
    pub follow: bool,
    #[arg(value_name = "PARTICIPANT")]
    pub participant: Option<String>,
    #[arg(value_name = "PROJECT_OR_ENTRY", long = "project")]
    pub target: Option<PathBuf>,
    #[arg(
        long,
        value_name = "ZENOH_ENDPOINT",
        help = "Read the execution at an explicit endpoint instead of this project's."
    )]
    pub endpoint: Option<String>,
}

impl Logs {
    pub async fn run(&self, app: &AppContext) -> Result<()> {
        crate::application::lifecycle::logs_command(
            app,
            self.target.as_deref(),
            self.endpoint.clone(),
            self.participant.clone(),
            self.follow,
        )
        .await
    }
}
