//! `phoxal stop` - end an existing execution.

use std::path::PathBuf;

use anyhow::Result;
use clap::Args;

use crate::cli::context::AppContext;

/// `stop` takes no endpoint.
///
/// It ends the processes this CLI started and recorded, which is a fact about
/// a project on this machine, not about an endpoint somewhere. The supervisor
/// has no stop command to send in the first place: it starts nothing, so it
/// stops nothing.
#[derive(Debug, Args)]
pub struct Stop {
    #[arg(value_name = "PROJECT")]
    target: Option<PathBuf>,
}

impl Stop {
    pub async fn run(&self, app: &AppContext) -> Result<()> {
        crate::application::lifecycle::stop_command(app, self.target.as_deref()).await
    }
}
