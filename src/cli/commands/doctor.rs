use anyhow::Result;
use clap::Args;

use crate::cli::AppContext;

#[derive(Debug, Args)]
pub struct Doctor {}

impl Doctor {
    pub async fn run(&self, app: &AppContext) -> Result<()> {
        crate::application::run::doctor_command(app).await
    }
}
