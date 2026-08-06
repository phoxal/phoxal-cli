//! `phoxal start` - create a fresh execution, wait for readiness, and exit.
//!
//! `start` runs the same build/publish/launch path as `run`; it differs only
//! in what it does once the daemon answers. It never mounts the TUI, and it
//! never becomes the supervisor: `phoxald` is a separate executable and the
//! systemd unit starts it directly, so readiness and the watchdog belong to
//! the daemon, not to this command (organization#978).

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
        crate::application::lifecycle::start_command(app, self.target.as_deref()).await
    }
}
