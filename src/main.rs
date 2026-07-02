use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::Parser;
use dotenvy::Error as DotenvError;
use phoxal::util::tracing_ansi_enabled;
use tracing_subscriber::EnvFilter;

use phoxal_cli::AppContext;
use phoxal_cli::Ui;
use phoxal_cli::commands::Cli;

#[tokio::main()]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            Ui::new().error(format!("{error:#}"));
            ExitCode::from(1)
        }
    }
}

async fn run() -> Result<()> {
    match dotenvy::dotenv() {
        Ok(_) => {}
        Err(DotenvError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("failed to load .env"),
    }

    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn"));
    tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .with_target(false)
        .without_time()
        .with_ansi(tracing_ansi_enabled())
        .init();

    let cli = Cli::parse();
    let workspace_root = cli.project_path.clone().unwrap_or(std::env::current_dir()?);
    let workspace_root = workspace_root.canonicalize().with_context(|| {
        format!(
            "failed to resolve project path as workspace root: {}",
            workspace_root.display()
        )
    })?;
    let app = AppContext::new(workspace_root, cli.catalog_source.clone())?;

    phoxal_cli::commands::dispatch(cli, &app).await
}
