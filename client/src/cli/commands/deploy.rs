//! Deploy source snapshots or prebuilt archives through one remote installer.

use std::path::PathBuf;

use anyhow::Result;
use clap::Args;

use crate::cli::AppContext;

#[derive(Debug, Args)]
pub struct Deploy {
    #[arg(value_name = "USER@HOST")]
    target: String,
    #[arg(
        value_name = "PROJECT",
        help = "Source project to snapshot. Defaults to the discovered project."
    )]
    project: Option<PathBuf>,
    #[arg(
        long,
        value_name = "BUILD_PHOXAL",
        conflicts_with = "project",
        help = "Push a prebuilt archive; the remote host needs no Cargo or Git."
    )]
    build: Option<PathBuf>,
}

impl Deploy {
    pub async fn run(&self, app: &AppContext) -> Result<()> {
        crate::application::run::deploy_command(
            app,
            self.target.clone(),
            self.project.clone(),
            self.build.clone(),
        )
        .await
    }
}
