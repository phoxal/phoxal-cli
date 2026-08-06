//! The client-owned Webots application process.
//!
//! Webots is launched, watched, and stopped by this client and by nothing else
//! (organization#978). The daemon has no Webots knowledge at all: the simulator
//! *participant* it launches is an ordinary staged binary from the bundle, and
//! the simulator *application* - the GUI, the world - is this process's.

use std::process::{Child, Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result};
use phoxal_cli_core::runtime::ParticipantSpec;

/// How long a SIGTERMed Webots is given to close its world before the
/// fallback. Webots writes state on the way out, so this is generous.
const GRACEFUL_BUDGET: Duration = Duration::from_secs(20);

/// How often the exit is polled while waiting.
const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// A running Webots application this client owns.
#[derive(Debug)]
pub(crate) struct Webots {
    child: Child,
}

impl Webots {
    /// Launch the Webots application from the spec simulation preparation
    /// produced.
    ///
    /// Its own process group, for the same reason the daemon gets one: a
    /// terminal Ctrl+C must reach the client's shutdown path, which then stops
    /// both processes in the right order - never the simulator directly, mid
    /// world write.
    pub(crate) fn launch(spec: &ParticipantSpec) -> Result<Self> {
        let mut command = Command::new(&spec.executable);
        command
            .args(&spec.args)
            .stdin(Stdio::null())
            .envs(spec.env.iter().cloned());
        if let Some(cwd) = &spec.cwd {
            command.current_dir(cwd);
        }
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        let child = command
            .spawn()
            .with_context(|| format!("failed to launch Webots ({})", spec.executable.display()))?;
        Ok(Self { child })
    }

    /// Whether Webots has already exited, and with what.
    pub(crate) fn exited(&mut self) -> Result<Option<std::process::ExitStatus>> {
        Ok(self.child.try_wait()?)
    }

    /// Stop Webots gracefully, falling back only if it will not go.
    ///
    /// SIGTERM, never SIGKILL first: Webots is closing a world, and killing it
    /// outright leaves the host with a crash report to dismiss rather than a
    /// clean exit.
    pub(crate) async fn stop(mut self) -> Result<()> {
        if self.child.try_wait()?.is_some() {
            return Ok(());
        }
        #[cfg(unix)]
        {
            // SAFETY: `kill` takes no pointer, and the pid is this process's
            // own direct child, which has not been reaped.
            unsafe {
                libc::kill(
                    i32::try_from(self.child.id()).unwrap_or(i32::MAX),
                    libc::SIGTERM,
                );
            }
        }
        let deadline = tokio::time::Instant::now() + GRACEFUL_BUDGET;
        loop {
            if self.child.try_wait()?.is_some() {
                return Ok(());
            }
            if tokio::time::Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
        tracing::warn!(
            "Webots did not exit within {}s of SIGTERM; killing it",
            GRACEFUL_BUDGET.as_secs()
        );
        self.child.kill().ok();
        self.child.wait()?;
        Ok(())
    }
}

impl Drop for Webots {
    /// A dropped handle must never leave a Webots running with no operator:
    /// the graceful path is [`Self::stop`], and this is the last resort for a
    /// panic or an early return that skipped it.
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(None)) {
            self.child.kill().ok();
            self.child.wait().ok();
        }
    }
}
