use std::path::PathBuf;

use anyhow::Result;
use clap::Args;

use crate::cli::AppContext;

#[derive(Debug, Args)]
pub struct Attach {
    #[arg(value_name = "PROJECT_OR_ENTRY")]
    target: Option<PathBuf>,
}

impl Attach {
    pub async fn run(&self, app: &AppContext) -> Result<()> {
        crate::application::attachment::attach_command(app, self.target.as_deref()).await
    }
}
