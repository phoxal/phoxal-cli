//! `phoxal` - the Phoxal project tool and disposable attached client.
//!
//! This is the half of the CLI pair that *builds*: it reads `robot.yaml`,
//! composes `extends`, resolves components and Cargo targets, invokes Cargo,
//! validates every selected binary's embedded compatibility metadata, and
//! publishes a finalized bundle. It then launches `phoxald` on that bundle and
//! attaches to it over the supervisor API (organization#978).
//!
//! It never supervises a graph itself. There is no in-process daemon path and
//! no private IPC: the daemon is a sibling executable, and the only channel to
//! a running execution is the execution-scoped Zenoh supervisor API.
//!
//! The command surface (see [`cli::commands`]) is:
//!
//! - `run`/`start`/`simulation webots run <world>` - build and publish a fresh
//!   bundle, launch `phoxald` on it, and (for `run`) attach;
//! - `attach`/`stop`/`status`/`logs` - existing executions only, no build and
//!   no project mutation;
//! - `build`/`validate`/`deploy`/`install`/`rollback`/`service`/`schema` - the
//!   project and host workflows;
//! - `doctor` - check host prerequisites; `self upgrade` - update the CLI pair.

#![allow(clippy::module_name_repetitions)]

mod application;
mod attach;
mod bootstrap;
mod cli;
mod cutover;
mod joypad;
mod lock;
mod reconcile;

use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::Parser;
use tracing_subscriber::EnvFilter;

use crate::cli::{AppContext, Cli, SessionAwareWriter, Ui, tracing_ansi_enabled};

#[tokio::main()]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    init_tracing();

    match run(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            if let Some(exit) = error.downcast_ref::<crate::cli::ReportedExit>() {
                return ExitCode::from(exit.0);
            }
            Ui::from_env().error(format!("{error:#}"));
            ExitCode::from(1)
        }
    }
}

fn init_tracing() {
    // Zenoh warns loudly and repeatedly when an endpoint is simply not there,
    // which is the ordinary "nothing is running" case for `attach`, `stop`,
    // `status`, and `logs` - and the command already says so precisely. Its
    // own errors still surface; `RUST_LOG` overrides all of this.
    let env_filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("warn,zenoh=error,zenoh_link_unixsock_stream=error"));
    // `SessionAwareWriter` routes every line through whichever `run`/
    // `simulation webots run` session (if any) has called `cli::output::diagnostics::install`
    // at the moment of the write, falling back to the normal stderr write
    // otherwise (see that module's own docs) - installing it here, ONCE for
    // the process's whole lifetime, is what keeps a live `tracing::warn!`
    // (e.g. a Zenoh connection retry) from writing straight to stderr
    // underneath an active TUI frame instead of through the renderer.
    //
    // Every `phoxal` invocation writes to a live console, so none of them
    // carries wall-clock timestamps: the daemon owns the log file that a line
    // is read from long after the fact, and it is a separate process with its
    // own subscriber.
    tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .with_target(false)
        .with_ansi(tracing_ansi_enabled())
        .with_writer(SessionAwareWriter)
        .without_time()
        .init();
}

async fn run(cli: Cli) -> Result<()> {
    let workspace_root = cli.project_path.clone().unwrap_or(std::env::current_dir()?);
    let workspace_root = workspace_root.canonicalize().with_context(|| {
        format!(
            "failed to resolve project path as workspace root: {}",
            workspace_root.display()
        )
    })?;
    let app = AppContext::new(workspace_root, cli.offline())?;

    crate::cli::dispatch(cli, &app).await
}
