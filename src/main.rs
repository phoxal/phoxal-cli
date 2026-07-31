use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::Parser;
use dotenvy::Error as DotenvError;
use tracing_subscriber::EnvFilter;

use phoxal_cli::cli::{AppContext, Cli, SessionAwareWriter, Ui, tracing_ansi_enabled};

#[tokio::main()]
async fn main() -> ExitCode {
    if let Some(exit) = phoxal_cli_supervisor::maybe_run_guardian() {
        return exit;
    }
    let cli = Cli::parse();
    init_tracing();

    match run(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            if let Some(exit) = error.downcast_ref::<phoxal_cli::cli::ReportedExit>() {
                return ExitCode::from(exit.0);
            }
            Ui::from_env().error(format!("{error:#}"));
            ExitCode::from(1)
        }
    }
}

fn init_tracing() {
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn"));
    // `SessionAwareWriter` routes every line through whichever `run`/
    // `simulation webots run` session (if any) has called `cli::output::diagnostics::install`
    // at the moment of the write, falling back to the normal stderr write
    // otherwise (see that module's own docs) - installing it here, ONCE for
    // the process's whole lifetime, is what actually closes findings A2/C2:
    // without this, `SessionAwareWriter`/`SessionWriter` were dead code and a
    // live `tracing::warn!` (e.g. a Zenoh connection retry) wrote straight to
    // stderr underneath an active TUI frame instead of through the renderer.
    let builder = tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .with_target(false)
        .with_ansi(tracing_ansi_enabled())
        .with_writer(SessionAwareWriter);
    // The private resident's stderr is the supervisor log file, where a line
    // is read long after the fact - keep wall-clock timestamps there. Every
    // other invocation writes to a live console and stays time-free.
    if phoxal_cli_supervisor::resident::has_private_bootstrap() {
        builder.init();
    } else {
        builder.without_time().init();
    }
}

async fn run(cli: Cli) -> Result<()> {
    match dotenvy::dotenv() {
        Ok(_) => {}
        Err(DotenvError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("failed to load .env"),
    }

    let workspace_root = cli.project_path.clone().unwrap_or(std::env::current_dir()?);
    let workspace_root = workspace_root.canonicalize().with_context(|| {
        format!(
            "failed to resolve project path as workspace root: {}",
            workspace_root.display()
        )
    })?;
    let app = AppContext::new(workspace_root, cli.offline())?;

    phoxal_cli::cli::dispatch(cli, &app).await
}
