use anyhow::Result;
use clap::Args;

use crate::cli::context::AppContext;

#[derive(Debug, Args)]
pub struct Doctor {}

impl Doctor {
    pub async fn run(&self, app: &AppContext) -> Result<()> {
        crate::application::doctor::doctor_command(app).await
    }
}
