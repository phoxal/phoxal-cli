//! Launching the framework `phoxal-supervisor` until a client can attach.
//!
//! The supervisor is a separate executable, never a mode of this one: `phoxal` can
//! never run the supervision loop in process. Everything this module knows
//! about the child is process facts and stderr - the completed Zenoh handshake
//! is readiness, and a socket file's existence proves nothing.
//!
//! The supervisor launched is always the one inside the release being executed.
//! That is the whole point
//! of a release owning its supervisor: the bundle and the binary that runs it
//! move together, so a local run and an installed run are the same launch.

use std::collections::HashSet;
use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};

/// How much of the supervisor's stderr is kept as early-exit evidence. It
/// writes its own log; this is only what a client shows when the child died
/// before there was anything to attach to.
const MAX_DIAGNOSTIC_BYTES: usize = 8 * 1024;

/// A launched supervisor and the stderr this client is capturing from it.
pub(crate) struct LaunchedSupervisor {
    child: Child,
    stderr: Arc<Mutex<String>>,
    /// The stderr pump. It ends when the pipe closes, which is when the child
    /// exits, so it is never joined - holding it keeps the handle owned rather
    /// than detached.
    _reader: Option<std::thread::JoinHandle<()>>,
}

impl LaunchedSupervisor {
    /// The supervisor's stderr so far, bounded.
    pub(crate) fn diagnostics(&self) -> String {
        self.stderr
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Whether the child has already exited, and with what.
    pub(crate) fn exited(&mut self) -> Result<Option<std::process::ExitStatus>> {
        Ok(self.child.try_wait()?)
    }

    /// The message an early exit earns: the exit status plus whatever the
    /// supervisor managed to say before it went.
    pub(crate) fn early_exit_message(&self, status: std::process::ExitStatus) -> String {
        let diagnostics = self.diagnostics();
        let diagnostics = diagnostics.trim();
        if diagnostics.is_empty() {
            format!("phoxal-supervisor exited with {status} before a client could attach")
        } else {
            format!(
                "phoxal-supervisor exited with {status} before a client could attach: {diagnostics}"
            )
        }
    }

    /// End a supervisor owned by a non-detachable simulation, including every
    /// participant process group below it.
    ///
    /// Ordinary `phoxal run` deliberately never calls this: its supervisor is
    /// durable and may outlive the client. A simulation is different because
    /// this client owns its world clock and therefore owns the whole local
    /// process group. Both waits are bounded so cleanup cannot hold the
    /// operator's terminal indefinitely.
    pub(crate) async fn terminate_owned(
        &mut self,
        graceful_budget: Duration,
        kill_budget: Duration,
    ) -> Result<()> {
        let supervisor_pid = i32::try_from(self.child.id())
            .map_err(|_| anyhow!("phoxal-supervisor process id does not fit a process group id"))?;
        let process_groups = owned_process_groups(supervisor_pid)?;

        signal_process_groups(&process_groups, libc::SIGTERM)?;
        if self
            .await_process_groups_exit(&process_groups, graceful_budget)
            .await?
        {
            return Ok(());
        }

        signal_process_groups(&process_groups, libc::SIGKILL)?;
        if self
            .await_process_groups_exit(&process_groups, kill_budget)
            .await?
        {
            return Ok(());
        }

        bail!(
            "a simulation-owned process group survived SIGKILL for {}s",
            kill_budget.as_secs()
        )
    }

    async fn await_process_groups_exit(
        &mut self,
        process_groups: &HashSet<libc::pid_t>,
        budget: Duration,
    ) -> Result<bool> {
        let deadline = tokio::time::Instant::now() + budget;
        loop {
            // `try_wait` is also the nonblocking reap for the group leader.
            let _ = self.child.try_wait()?;
            if process_groups
                .iter()
                .map(|group| process_group_exists(*group))
                .collect::<Result<Vec<_>>>()?
                .into_iter()
                .all(|exists| !exists)
            {
                return Ok(true);
            }
            if tokio::time::Instant::now() >= deadline {
                return Ok(false);
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

fn owned_process_groups(supervisor_pid: libc::pid_t) -> Result<HashSet<libc::pid_t>> {
    let mut system = System::new();
    system.refresh_processes_specifics(ProcessesToUpdate::All, true, ProcessRefreshKind::nothing());
    let parents = system
        .processes()
        .iter()
        .map(|(pid, process)| (pid.as_u32(), process.parent().map(Pid::as_u32)))
        .collect::<Vec<_>>();
    let descendants = descendant_pids(supervisor_pid as u32, &parents);

    let mut groups = HashSet::from([supervisor_pid]);
    for descendant in descendants {
        let pid = i32::try_from(descendant)
            .context("simulation child process id does not fit a process group id")?;
        let group = unsafe { libc::getpgid(pid) };
        if group > 0 {
            groups.insert(group);
            continue;
        }
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::ESRCH) {
            return Err(error).context("failed to inspect a simulation child process group");
        }
    }
    Ok(groups)
}

fn descendant_pids(supervisor_pid: u32, parents: &[(u32, Option<u32>)]) -> HashSet<u32> {
    let mut descendants = HashSet::new();
    loop {
        let before = descendants.len();
        for (pid, parent) in parents {
            if parent
                .is_some_and(|parent| parent == supervisor_pid || descendants.contains(&parent))
            {
                descendants.insert(*pid);
            }
        }
        if descendants.len() == before {
            break;
        }
    }
    descendants
}

fn signal_process_groups(process_groups: &HashSet<libc::pid_t>, signal: libc::c_int) -> Result<()> {
    for process_group in process_groups {
        signal_process_group(*process_group, signal)?;
    }
    Ok(())
}

fn signal_process_group(process_group: libc::pid_t, signal: libc::c_int) -> Result<()> {
    let result = unsafe { libc::kill(-process_group, signal) };
    if result == 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        return Ok(());
    }
    Err(error).context("failed to signal the owned phoxal-supervisor process group")
}

fn process_group_exists(process_group: libc::pid_t) -> Result<bool> {
    let result = unsafe { libc::kill(-process_group, 0) };
    if result == 0 {
        return Ok(true);
    }
    let error = std::io::Error::last_os_error();
    match error.raw_os_error() {
        Some(libc::ESRCH) => Ok(false),
        Some(libc::EPERM) => Ok(true),
        _ => Err(error).context("failed to inspect the owned phoxal-supervisor process group"),
    }
}

/// Spawn `<release>/phoxal-supervisor <release>/bundle`.
///
/// The supervisor is placed in its own process group. That is not tidiness: a
/// Ctrl+C in the terminal running this client goes to the client's foreground
/// process group, and the supervisor is durable - it must survive the client that
/// started it, because detaching is not stopping.
pub(crate) fn spawn(release: &phoxal_cli_project::ReleaseLayout) -> Result<LaunchedSupervisor> {
    // The supervisor and bundle come from the same verified release, so the
    // argv this builds cannot pair one release's supervisor with another's bundle.
    // The supervisor still receives a bundle root and nothing else.
    let mut command = Command::new(&release.supervisor);
    command
        .arg(&release.bundle)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    isolate_process_group(&mut command);
    let mut child = command.spawn().with_context(|| {
        format!(
            "failed to launch the supervisor {} on {}; a deployment release carries the \
             supervisor that runs it, so rebuild the release if it is missing",
            release.supervisor.display(),
            release.bundle.display(),
        )
    })?;

    let stderr = Arc::new(Mutex::new(String::new()));
    let reader = child.stderr.take().map(|mut pipe| {
        let sink = Arc::clone(&stderr);
        // A blocking thread rather than a task: this pipe outlives nothing and
        // races nothing, and reading it must not depend on the async runtime
        // still being scheduled while the client is tearing down.
        std::thread::spawn(move || {
            let mut buffer = [0_u8; 4_096];
            loop {
                match pipe.read(&mut buffer) {
                    Ok(0) | Err(_) => return,
                    Ok(read) => {
                        let mut sink = sink
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        if sink.len() < MAX_DIAGNOSTIC_BYTES {
                            sink.push_str(&String::from_utf8_lossy(&buffer[..read]));
                            sink.truncate(MAX_DIAGNOSTIC_BYTES);
                        }
                    }
                }
            }
        })
    });

    Ok(LaunchedSupervisor {
        child,
        stderr,
        _reader: reader,
    })
}

fn isolate_process_group(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    // `setpgid(0, 0)` in the child: it leaves this client's foreground process
    // group, so a terminal SIGINT never reaches it.
    command.process_group(0);
}

#[cfg(test)]
mod tests {
    use super::descendant_pids;

    #[test]
    fn forced_simulation_cleanup_discovers_each_nested_owned_process() {
        let tree = [
            (20, Some(10)),
            (30, Some(20)),
            (40, Some(30)),
            (99, Some(1)),
        ];
        let descendants = descendant_pids(10, &tree);
        assert_eq!(descendants.len(), 3);
        assert!(descendants.contains(&20));
        assert!(descendants.contains(&30));
        assert!(descendants.contains(&40));
        assert!(!descendants.contains(&99));
    }
}
