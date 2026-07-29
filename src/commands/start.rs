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

use crate::AppContext;
use crate::commands::resident::resolve_target;
use crate::run::{DriversMode, RunOptions, run_resident_supervision, wait_for_required_readiness};
use phoxal_cli_supervisor::systemd::notify::SdNotify;

#[derive(Debug, Args)]
pub struct Start {
    #[arg(value_name = "ROOT_OR_ENTRY")]
    pub target: Option<PathBuf>,
}

impl Start {
    pub async fn run(&self, app: &AppContext) -> Result<()> {
        // `start` is headless: the full official driver set and no subset,
        // no TUI. Driver gating lives on `run`; a robot instance always launches
        // its drivers.
        let options = RunOptions {
            drivers: DriversMode::On,
            drivers_subset: Vec::new(),
            offline: app.offline,
        };
        let target = resolve_target(self.target.as_deref(), app.project.root())?;
        // SAFETY: command dispatch has not started worker threads for this run
        // yet; every project-local path helper must agree on the selected root,
        // and the detached child inherits it.
        unsafe {
            std::env::set_var(phoxal_cli_project::PROJECT_ROOT_ENV, &target.project);
        }

        // The detached child of an interactive `start` re-execs `phoxal start`
        // with the private bootstrap descriptor set: it runs the resident in
        // process and reports the bootstrap result back to the launcher. It owns
        // no notify socket.
        if phoxal_cli_supervisor::resident::has_private_bootstrap() {
            return run_resident_supervision(app, target.project, options, None).await;
        }

        // Under systemd (`Type=notify`) stay the in-process foreground resident
        // that owns `sd_notify`/watchdog; the detached path is never taken.
        if let Some(notify) = SdNotify::from_env()? {
            return run_resident_supervision(app, target.project, options, Some(notify)).await;
        }

        // Interactive, non-systemd: behave like `run -d`. Spawn the detached
        // resident, wait for required readiness, print how to reach it, return.
        self.start_detached(app, &target.project).await
    }

    async fn start_detached(&self, app: &AppContext, project: &std::path::Path) -> Result<()> {
        let (mut launched, feed, _) =
            crate::run::connect_to_detached_resident_feed(project).await?;
        wait_for_required_readiness(&feed, &mut launched.child).await?;
        let display = project.display();
        app.ui.info(format!(
            "robot instance ready; attach with `phoxal attach {display}` or stop with `phoxal stop {display}`"
        ));
        Ok(())
    }
}
