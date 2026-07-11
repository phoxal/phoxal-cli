use anyhow::Result;
use clap::Args;

use crate::AppContext;

#[derive(Debug, Args)]
pub struct Doctor {}

impl Doctor {
    pub async fn run(&self, app: &AppContext) -> Result<()> {
        crate::host_doctor::report(app);
        Ok(())
    }
}
