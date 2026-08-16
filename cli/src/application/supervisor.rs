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
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};

/// How much of the supervisor's log is read back to find its last words.
const MAX_DIAGNOSTIC_BYTES: u64 = 8 * 1024;

/// How many of those lines a client quotes. A supervisor says why it is going
/// in its last lines and logs everything else along the way, so a message that
/// quoted the whole tail would bury the reason under the run that preceded it.
/// The full file is on disk, and every failure block names it.
const MAX_DIAGNOSTIC_LINES: usize = 3;

/// A launched supervisor and the log file it is writing its stderr into.
pub(crate) struct LaunchedSupervisor {
    child: Child,
    log: PathBuf,
}

impl LaunchedSupervisor {
    /// The supervisor's last words: the final lines of its log.
    ///
    /// Read from the file rather than from a pipe this client pumps: an
    /// ordinary `run` supervisor outlives the client that started it, so a
    /// client-owned pipe would end its log at detach - and leave nothing at all
    /// to read after any exit.
    pub(crate) fn diagnostics(&self) -> String {
        let tail = read_tail(&self.log, MAX_DIAGNOSTIC_BYTES).unwrap_or_default();
        last_lines(&tail, MAX_DIAGNOSTIC_LINES)
    }

    /// Whether the child has already exited, and with what.
    pub(crate) fn exited(&mut self) -> Result<Option<std::process::ExitStatus>> {
        Ok(self.child.try_wait()?)
    }

    /// The message an early exit earns: the exit status plus whatever the
    /// supervisor managed to say before it went.
    pub(crate) fn early_exit_message(&self, status: std::process::ExitStatus) -> String {
        self.exit_message(status, " before a client could attach")
    }

    /// The same, for a supervisor that answered and then gave up during
    /// startup. It is a different sentence because it is a different event:
    /// the client did attach, and watched the graph fail to come up.
    pub(crate) fn startup_exit_message(&self, status: std::process::ExitStatus) -> String {
        self.exit_message(status, " while its graph was starting")
    }

    fn exit_message(&self, status: std::process::ExitStatus, during: &str) -> String {
        let diagnostics = self.diagnostics();
        let diagnostics = diagnostics.trim();
        if diagnostics.is_empty() {
            format!("phoxal-supervisor exited with {status}{during}")
        } else {
            format!("phoxal-supervisor exited with {status}{during}: {diagnostics}")
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

/// Spawn `<release>/phoxal-supervisor <release>/bundle`, with its standard
/// error retained at `log`.
///
/// The supervisor is placed in its own process group. That is not tidiness: a
/// Ctrl+C in the terminal running this client goes to the client's foreground
/// process group, and the supervisor is durable - it must survive the client that
/// started it, because detaching is not stopping. The same durability is why
/// the child owns the log file descriptor outright: there is no client left to
/// pump a pipe once the operator detaches.
pub(crate) fn spawn(
    release: &phoxal_cli_project::ReleaseLayout,
    log: &Path,
) -> Result<LaunchedSupervisor> {
    let file = create_log(log)?;
    let errors = file
        .try_clone()
        .with_context(|| format!("failed to open {} for the supervisor", log.display()))?;
    // The supervisor and bundle come from the same verified release, so the
    // argv this builds cannot pair one release's supervisor with another's bundle.
    // The supervisor still receives a bundle root and nothing else.
    let mut command = Command::new(&release.supervisor);
    command
        .arg(&release.bundle)
        .stdin(Stdio::null())
        .stdout(Stdio::from(file))
        .stderr(Stdio::from(errors));
    isolate_process_group(&mut command);
    let child = command.spawn().with_context(|| {
        format!(
            "failed to launch the supervisor {} on {}; a deployment release carries the \
             supervisor that runs it, so rebuild the release if it is missing",
            release.supervisor.display(),
            release.bundle.display(),
        )
    })?;

    Ok(LaunchedSupervisor {
        child,
        log: log.to_path_buf(),
    })
}

/// Truncate and open one per-session log. Each launch starts a fresh file: the
/// log describes the execution being started, not every execution this project
/// ever had.
pub(crate) fn create_log(path: &Path) -> Result<std::fs::File> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    std::fs::File::create(path).with_context(|| format!("failed to create {}", path.display()))
}

/// The last `limit` lines of `text` worth quoting.
///
/// A supervisor's log is mostly narration - its own and every participant's,
/// relayed - and the tail of a dying one is dominated by transport teardown at
/// INFO. What an operator needs is what the supervisor *complained* about, so
/// the narration is skipped when anything else is there, and only used when the
/// supervisor left nothing louder behind.
fn last_lines(text: &str, limit: usize) -> String {
    let lines = text
        .lines()
        .map(str::trim_end)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();
    let notable = lines
        .iter()
        .copied()
        .filter(|line| !is_narration(line))
        .collect::<Vec<_>>();
    let quoted = if notable.is_empty() { &lines } else { &notable };
    quoted[quoted.len().saturating_sub(limit)..].join("\n")
}

fn is_narration(line: &str) -> bool {
    [" INFO ", " DEBUG ", " TRACE "]
        .iter()
        .any(|level| line.contains(level))
}

/// The last `budget` bytes of `path`, decoded lossily and starting at a line
/// boundary so a truncated first line is never quoted as evidence.
fn read_tail(path: &Path, budget: u64) -> Option<String> {
    let mut file = std::fs::File::open(path).ok()?;
    let length = file.metadata().ok()?.len();
    let start = length.saturating_sub(budget);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).ok()?;
    let text = String::from_utf8_lossy(&bytes).into_owned();
    if start == 0 {
        return Some(text);
    }
    Some(
        text.split_once('\n')
            .map_or(text.as_str(), |(_, rest)| rest)
            .to_string(),
    )
}

fn isolate_process_group(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    // `setpgid(0, 0)` in the child: it leaves this client's foreground process
    // group, so a terminal SIGINT never reaches it.
    command.process_group(0);
}

#[cfg(test)]
mod tests {
    use super::{descendant_pids, last_lines, read_tail};

    /// A supervisor's message is its last words, not its whole run: the log is
    /// on disk for the rest.
    #[test]
    fn only_the_last_lines_are_quoted_as_evidence() {
        let log = "starting\n\nlaunching participants\nstage stalled: front_camera failed\n";
        assert_eq!(
            last_lines(log, 3),
            "starting\nlaunching participants\nstage stalled: front_camera failed"
        );
        assert_eq!(last_lines(log, 1), "stage stalled: front_camera failed");
        assert_eq!(last_lines("", 3), "");
    }

    /// The teardown narration a dying supervisor ends on must not push its own
    /// complaint out of the quote.
    #[test]
    fn narration_never_crowds_out_what_the_supervisor_complained_about() {
        let log = concat!(
            "2026-08-15T22:14:55.900000Z ERROR phoxal_supervisor: required startup phase failed\n",
            "2026-08-15T22:14:55.962631Z  INFO transport::session: close session zid=bd90\n",
            "2026-08-15T22:14:55.962764Z  INFO transport::session: close session zid=5a6a\n",
            "phoxal-supervisor: stage 'starting robot graph' stalled: front_camera\n",
        );
        let quoted = last_lines(log, 3);
        assert!(quoted.contains("required startup phase failed"), "{quoted}");
        assert!(quoted.contains("stalled: front_camera"), "{quoted}");
        assert!(!quoted.contains("close session"), "{quoted}");

        // A supervisor that only ever narrated still says something.
        let quiet = "2026-08-15T22:14:55.962631Z  INFO transport::session: close session\n";
        assert!(last_lines(quiet, 3).contains("close session"));
    }

    /// The tail is bounded and starts on a line boundary, so a long-running
    /// supervisor's evidence is the end of its log and never half a line.
    #[test]
    fn the_diagnostic_tail_is_bounded_and_starts_at_a_line_boundary() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("supervisor.log");
        std::fs::write(&path, "first line\nsecond line\nthird line\n").unwrap();

        assert_eq!(
            read_tail(&path, 1_024).unwrap(),
            "first line\nsecond line\nthird line\n"
        );
        assert_eq!(read_tail(&path, 20).unwrap(), "third line\n");
        assert!(read_tail(&directory.path().join("absent.log"), 1_024).is_none());
    }

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
