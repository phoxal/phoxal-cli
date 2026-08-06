//! The headless robot-instance verb `phoxal start` (#936).
//!
//! `start` runs the same universal pipeline as `run` - classify the root, refresh
//! staging when it is a source project, then supervise the staged layout - but it
//! is headless: it never mounts the TUI or takes interactive flags.
//! It has two invocation modes:
//!
//! - **interactive** (no `NOTIFY_SOCKET`): behaves like `run -d`. It spawns the
//!   detached resident, waits for required startup readiness, prints how to attach
//!   and stop, and returns.
//! - **under systemd** (`NOTIFY_SOCKET` set, `Type=notify`): it stays the
//!   in-process foreground resident that owns `sd_notify`. After required
//!   readiness it sends `READY=1`, and while it runs it pings `WATCHDOG=1` at half
//!   the `WATCHDOG_USEC` interval. `phoxal.service` uses
//!   `ExecStart=phoxal start /var/phoxal` (#930).
//!
//! Every spawned child (the router and all participants) has `NOTIFY_SOCKET` and
//! the watchdog variables removed from its environment - only the resident owns
//! notify authority - which `ManagedChild`'s environment scrub does at the single
//! spawn boundary for both modes.

use std::path::PathBuf;

use anyhow::Result;
use clap::Args;

use crate::cli::AppContext;

#[derive(Debug, Args)]
pub struct Start {
    #[arg(value_name = "ROOT_OR_ENTRY")]
    pub target: Option<PathBuf>,
}

impl Start {
    pub async fn run(&self, app: &AppContext) -> Result<()> {
        crate::application::run::start_command(app, self.target.as_deref()).await
    }
}
