//! The client-owned Webots application process.
//!
//! Webots is launched, watched, and stopped by this client and by nothing else.
//! The supervisor has no Webots knowledge at all: the simulator
//! *participant* it launches is an ordinary staged binary from the bundle, and
//! the simulator *application* - the GUI, the world - is this process's.

use std::num::NonZeroI32;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use phoxal_cli_project::WebotsLaunch;

/// How long a SIGTERMed Webots is given to close its world before the
/// fallback. Webots writes state on the way out, so this is generous.
const GRACEFUL_BUDGET: Duration = Duration::from_secs(20);

const SHUTDOWN: [(libc::c_int, Duration); 2] = [
    (libc::SIGTERM, GRACEFUL_BUDGET),
    (libc::SIGKILL, Duration::from_secs(1)),
];

/// How often the exit is polled while waiting.
const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// A running Webots application this client owns.
#[derive(Debug)]
pub(crate) struct Webots {
    child: Child,
    process_group: Option<NonZeroI32>,
}

impl Webots {
    /// Launch the Webots application from the spec simulation preparation
    /// produced, with its output retained at `log`.
    ///
    /// Its own process group, for the same reason the supervisor gets one: a
    /// terminal Ctrl+C must reach the client's shutdown path, which then stops
    /// both processes in the right order - never the simulator directly, mid
    /// world write.
    ///
    /// Neither stream is inherited. Webots writes driver notices
    /// (`UNSUPPORTED (log once): ...`) whenever it feels like it, and an
    /// inherited stream puts them straight over the dashboard's frame. The
    /// controller's own output arrives on these same streams - Webots is
    /// launched with `--stdout --stderr` - so this file is where a simulation
    /// that failed is read back from.
    pub(crate) fn launch(spec: &WebotsLaunch, log: &std::path::Path) -> Result<Self> {
        let file = crate::application::supervisor::create_log(log)?;
        let errors = file
            .try_clone()
            .with_context(|| format!("failed to open {} for Webots", log.display()))?;
        let mut command = Command::new(&spec.executable);
        command
            .args(&spec.args)
            .stdin(Stdio::null())
            .stdout(Stdio::from(file))
            .stderr(Stdio::from(errors));
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        let child = command
            .spawn()
            .with_context(|| format!("failed to launch Webots ({})", spec.executable.display()))?;
        let process_group = Some(
            NonZeroI32::new(
                libc::pid_t::try_from(child.id())
                    .context("Webots process id does not fit in a Unix process-group id")?,
            )
            .context("Webots process group must have a positive id")?,
        );
        Ok(Self {
            child,
            process_group,
        })
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
        let process_group = self
            .process_group
            .context("Webots process group ownership was already released")?;
        for (signal, budget) in SHUTDOWN {
            signal_process_group(process_group, signal)?;
            let deadline = tokio::time::Instant::now() + budget;
            loop {
                if !process_group_alive(&mut self.child, process_group)? {
                    self.process_group = None;
                    return Ok(());
                }
                let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
                if remaining.is_zero() {
                    break;
                }
                tokio::time::sleep(POLL_INTERVAL.min(remaining)).await;
            }
            if signal == libc::SIGKILL {
                bail!("Webots process group remained alive after SIGKILL");
            }
            tracing::warn!(
                "Webots did not exit within {}s of SIGTERM; killing its process group",
                GRACEFUL_BUDGET.as_secs()
            );
        }
        unreachable!("the shutdown signal sequence is non-empty")
    }
}

impl Drop for Webots {
    /// A dropped handle must never leave a Webots running with no operator:
    /// the graceful path is [`Self::stop`], and this is the last resort for a
    /// panic or an early return that skipped it.
    fn drop(&mut self) {
        if let Some(process_group) = self.process_group.take() {
            let _ = signal_process_group(process_group, libc::SIGKILL);
        }
        let _ = self.child.try_wait();
    }
}

fn signal_process_group(process_group: NonZeroI32, signal: libc::c_int) -> Result<()> {
    // SAFETY: `kill` takes no pointer. The negative pid targets exactly the
    // process group created for the direct Webots child at launch.
    if unsafe { libc::kill(-process_group.get(), signal) } == 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        return Ok(());
    }
    Err(error).context("failed to signal the Webots process group")
}

fn process_group_alive(child: &mut Child, process_group: NonZeroI32) -> Result<bool> {
    // Reap the direct child before probing: an unreaped group-leader zombie can
    // otherwise make `kill(-pgid, 0)` look alive until the graceful budget ends.
    let _ = child.try_wait()?;
    if unsafe { libc::kill(-process_group.get(), 0) } == 0 {
        return Ok(true);
    }
    let error = std::io::Error::last_os_error();
    match error.raw_os_error() {
        Some(libc::ESRCH) => Ok(false),
        Some(libc::EPERM) => Ok(true),
        _ => Err(error).context("failed to inspect the Webots process group"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shutdown_targets_the_group_with_term_then_kill() {
        assert_eq!(-NonZeroI32::new(42).unwrap().get(), -42);
        assert_eq!(SHUTDOWN[0].0, libc::SIGTERM);
        assert_eq!(SHUTDOWN[1].0, libc::SIGKILL);
    }
}
