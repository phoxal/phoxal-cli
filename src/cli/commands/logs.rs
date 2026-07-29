use anyhow::Result;
use clap::Args;

use crate::cli::AppContext;
use crate::cli::commands::status::BusTargetArgs;

#[derive(Debug, Args)]
pub struct Logs {
    #[arg(short = 'f', long, help = "Follow log events until interrupted.")]
    pub follow: bool,
    #[arg(value_name = "PARTICIPANT")]
    pub participant: Option<String>,
    #[command(flatten)]
    pub target: BusTargetArgs,
}

impl Logs {
    pub async fn run(&self, app: &AppContext) -> Result<()> {
        crate::application::attachment::logs_command(
            app,
            &self.target.request(),
            self.participant.clone(),
            self.follow,
        )
        .await
    }
}
