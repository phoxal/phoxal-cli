//! The local launcher: the CLI starts the robot's runtimes itself.
//!
//! The supervisor observes; it never spawns. So on a developer's machine the
//! process that starts the graph is this one, and it is therefore the process
//! that knows why a runtime is not there - an exec failure, a panic, an exit
//! status, a log file. That knowledge is the whole reason the launcher exists:
//! the snapshot can only say *absent*, and "absent because `bin/ddsm115` exited
//! with status 1, see `.phoxal/run/log/left_drive.log`" is what an operator
//! actually needs.
//!
//! Every child is its own process group with no inherited pipes and `stdin`
//! closed, so `phoxal start` can leave a running robot behind and a Ctrl+C in
//! this terminal reaches only this client. What ties the session together
//! afterwards is [`SessionRecord`] at `.phoxal/run/session.json`: pids, ids, and
//! log paths, written once the graph is up and consumed by `stop` - from this
//! terminal or any other.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// How often a signalled process is re-checked while waiting for it to go.
const REAP_INTERVAL: Duration = Duration::from_millis(50);

/// Which of the robot's runtimes a session starts.
///
/// This is a launch decision and only a launch decision: the bundle contains
/// every runtime whatever is chosen here, and choosing differently next time
/// requires no rebuild.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Selection {
    /// Everything the manifest declares.
    All,
    /// No component drivers - `--drivers off`, and what a simulation uses,
    /// because Webots supplies the components instead.
    DriversOff,
    /// Only these component-instance drivers, plus the brain and every service.
    Drivers(BTreeSet<String>),
}

impl Selection {
    /// Resolve the selection from the two flags, rejecting a `--driver` id the
    /// robot does not declare.
    pub(crate) fn resolve(
        drivers_off: bool,
        subset: Vec<String>,
        available: &BTreeSet<String>,
    ) -> Result<Self> {
        if drivers_off {
            anyhow::ensure!(
                subset.is_empty(),
                "--drivers off and --driver select opposite things; pass one"
            );
            return Ok(Self::DriversOff);
        }
        if subset.is_empty() {
            return Ok(Self::All);
        }
        let subset = subset.into_iter().collect::<BTreeSet<_>>();
        let unknown = subset.difference(available).cloned().collect::<Vec<_>>();
        anyhow::ensure!(
            unknown.is_empty(),
            "unknown driver id(s): {}; this robot declares drivers for: {}",
            unknown.join(", "),
            if available.is_empty() {
                "<none>".to_string()
            } else {
                available.iter().cloned().collect::<Vec<_>>().join(", ")
            }
        );
        Ok(Self::Drivers(subset))
    }

    fn includes(&self, runtime: &phoxal_cli_project::RobotRuntime) -> bool {
        if runtime.role != phoxal_cli_project::RuntimeRole::Driver {
            return true;
        }
        match self {
            Self::All => true,
            Self::DriversOff => false,
            Self::Drivers(selected) => selected.contains(&runtime.participant_id),
        }
    }

    /// The drivers this selection deliberately leaves out, for the one advisory
    /// line that explains why hardware rows are absent.
    pub(crate) fn excluded(&self, available: &BTreeSet<String>) -> Vec<String> {
        match self {
            Self::All => Vec::new(),
            Self::DriversOff => available.iter().cloned().collect(),
            Self::Drivers(selected) => available.difference(selected).cloned().collect(),
        }
    }
}

/// One runtime this session started.
#[derive(Debug)]
pub(crate) struct LaunchedRuntime {
    pub(crate) participant: String,
    pub(crate) log: PathBuf,
    child: Child,
}

impl LaunchedRuntime {
    pub(crate) fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Whether this child has already exited, and with what.
    pub(crate) fn exited(&mut self) -> Result<Option<std::process::ExitStatus>> {
        Ok(self.child.try_wait()?)
    }
}

/// Everything one launched runtime needs on its command line.
///
/// There is nothing else. No execution id - the router's session id is the
/// execution, and `phoxal-bus` reads it. No time origin - real robot time zero
/// is the host boot. No shutdown grace - a runtime exits on SIGTERM.
fn argv(runtime: &str, bundle_root: &Path, endpoint: &str, simulation: bool) -> Vec<String> {
    let mut argv = vec![
        "--participant-id".to_string(),
        runtime.to_string(),
        "--bundle-root".to_string(),
        bundle_root.display().to_string(),
        "--connect".to_string(),
        endpoint.to_string(),
    ];
    if simulation {
        argv.push("--simulation".to_string());
    }
    argv
}

/// Where one runtime's output is retained for this session.
pub(crate) fn runtime_log(paths: &phoxal_cli_host::paths::RuntimePaths, runtime: &str) -> PathBuf {
    paths.volatile_root().join("log").join(format!("{runtime}.log"))
}

/// Start every selected runtime of `bundle_root` against `endpoint`.
///
/// A failure part-way through leaves the already-started children running: the
/// caller owns them from the moment this returns either way, and it is the
/// caller that has the record to stop them with.
pub(crate) fn launch(
    bundle_root: &Path,
    endpoint: &str,
    simulation: bool,
    selection: &Selection,
    paths: &phoxal_cli_host::paths::RuntimePaths,
) -> Result<Vec<LaunchedRuntime>> {
    let runtimes = phoxal_cli_project::bundle_runtimes(bundle_root)?;
    let mut launched = Vec::new();
    for runtime in runtimes.iter().filter(|runtime| selection.includes(runtime)) {
        let log = runtime_log(paths, &runtime.participant_id);
        match spawn_one(bundle_root, endpoint, simulation, runtime, &log) {
            Ok(child) => launched.push(child),
            Err(error) => return Err(error),
        }
    }
    Ok(launched)
}

fn spawn_one(
    bundle_root: &Path,
    endpoint: &str,
    simulation: bool,
    runtime: &phoxal_cli_project::RobotRuntime,
    log: &Path,
) -> Result<LaunchedRuntime> {
    use std::os::unix::process::CommandExt;

    let executable = bundle_root.join(phoxal_bundle::BIN_DIR).join(&runtime.binary);
    let file = create_log(log)?;
    let errors = file
        .try_clone()
        .with_context(|| format!("failed to open {} for {}", log.display(), runtime.binary))?;
    let mut command = Command::new(&executable);
    command
        .args(argv(
            &runtime.participant_id,
            bundle_root,
            endpoint,
            simulation,
        ))
        .stdin(Stdio::null())
        .stdout(Stdio::from(file))
        .stderr(Stdio::from(errors));
    // Its own process group, so a Ctrl+C in this terminal reaches only this
    // client and `phoxal start` can leave the robot running behind it.
    command.process_group(0);
    let child = command.spawn().with_context(|| {
        format!(
            "failed to launch runtime '{}' from {}",
            runtime.participant_id,
            executable.display()
        )
    })?;
    Ok(LaunchedRuntime {
        participant: runtime.participant_id.clone(),
        log: log.to_path_buf(),
        child,
    })
}

/// Truncate and open one per-session log. Each launch starts a fresh file: the
/// log describes the execution being started, not every execution this project
/// ever had.
fn create_log(path: &Path) -> Result<std::fs::File> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    std::fs::File::create(path).with_context(|| format!("failed to create {}", path.display()))
}

// ---------------------------------------------------------------------------
// the session record
// ---------------------------------------------------------------------------

/// What this CLI started, written down so any terminal can stop it.
///
/// It is the CLI's own bookkeeping and no part of any wire contract: nothing
/// else reads it, and it never leaves the project.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "schema")]
pub(crate) enum SessionDocument {
    #[serde(rename = "phoxal/cli-session/v0")]
    V0(SessionRecord),
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct SessionRecord {
    /// The project or installed root this session runs out of.
    pub(crate) project: PathBuf,
    /// The endpoint the supervisor bound and every runtime dialled.
    pub(crate) endpoint: String,
    /// The bundle every recorded process was launched against.
    pub(crate) bundle: PathBuf,
    /// Whether the runtimes were started with `--simulation`.
    pub(crate) simulation: bool,
    pub(crate) supervisor: RecordedProcess,
    /// Ordered by participant id, like the supervisor's own process rows.
    pub(crate) runtimes: Vec<RecordedRuntime>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct RecordedProcess {
    pub(crate) pid: u32,
    pub(crate) log: PathBuf,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct RecordedRuntime {
    pub(crate) participant: String,
    pub(crate) pid: u32,
    pub(crate) log: PathBuf,
}

/// Where one project's session record lives.
pub(crate) fn record_path(paths: &phoxal_cli_host::paths::RuntimePaths) -> PathBuf {
    paths.volatile_root().join("session.json")
}

impl SessionRecord {
    pub(crate) fn write(&self, paths: &phoxal_cli_host::paths::RuntimePaths) -> Result<()> {
        let path = record_path(paths);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let document = SessionDocument::V0(self.clone());
        std::fs::write(&path, serde_json::to_vec_pretty(&document)?)
            .with_context(|| format!("failed to write {}", path.display()))
    }

    /// Read the record for `paths`, or `None` when this project has no session.
    ///
    /// A record that cannot be parsed is `None` too, deliberately: it is this
    /// CLI's own scratch bookkeeping, and a stale or hand-edited one must not
    /// turn every later command into an error.
    pub(crate) fn read(paths: &phoxal_cli_host::paths::RuntimePaths) -> Option<Self> {
        let bytes = std::fs::read(record_path(paths)).ok()?;
        match serde_json::from_slice::<SessionDocument>(&bytes) {
            Ok(SessionDocument::V0(record)) => Some(record),
            Err(error) => {
                tracing::debug!("ignoring an unreadable session record: {error}");
                None
            }
        }
    }

    pub(crate) fn remove(paths: &phoxal_cli_host::paths::RuntimePaths) -> Result<()> {
        match std::fs::remove_file(record_path(paths)) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error).with_context(|| {
                format!("failed to remove {}", record_path(paths).display())
            }),
        }
    }

    /// The log this session retained for `participant`, if it started it.
    pub(crate) fn log_for(&self, participant: &str) -> Option<&Path> {
        self.runtimes
            .iter()
            .find(|runtime| runtime.participant == participant)
            .map(|runtime| runtime.log.as_path())
    }
}

// ---------------------------------------------------------------------------
// stopping
// ---------------------------------------------------------------------------

/// How long every recorded runtime is given to exit on SIGTERM before it is
/// killed. A runtime that will not stop must not hold the operator's terminal.
const RUNTIME_GRACE: Duration = Duration::from_secs(10);

/// The same, for the supervisor, which is asked to go only after the graph has.
const SUPERVISOR_GRACE: Duration = Duration::from_secs(10);

/// End the session `record` describes and consume it.
///
/// Runtimes first, then the supervisor: the supervisor is the router every
/// runtime is talking through, so taking it away first would leave every child
/// tearing down a transport that has already gone.
pub(crate) async fn stop_session(
    record: &SessionRecord,
    paths: &phoxal_cli_host::paths::RuntimePaths,
) -> Result<()> {
    let runtimes = record
        .runtimes
        .iter()
        .map(|runtime| runtime.pid)
        .collect::<Vec<_>>();
    terminate(&runtimes, RUNTIME_GRACE).await?;
    terminate(&[record.supervisor.pid], SUPERVISOR_GRACE).await?;
    SessionRecord::remove(paths)
}

/// SIGTERM every process group, wait out `grace`, then SIGKILL whatever is left.
///
/// Each pid is a process-group leader by construction - every child is spawned
/// with `setpgid(0, 0)` - so signalling the group reaches anything a runtime
/// started for itself, without walking the process tree for descendants nobody
/// recorded.
async fn terminate(pids: &[u32], grace: Duration) -> Result<()> {
    signal_all(pids, libc::SIGTERM)?;
    if await_exit(pids, grace).await? {
        return Ok(());
    }
    signal_all(pids, libc::SIGKILL)?;
    // A process that survives SIGKILL is unkillable (uninterruptible I/O), and
    // there is nothing further this client can do about it, so the wait is
    // bounded and its outcome reported rather than retried forever.
    if await_exit(pids, grace).await? {
        return Ok(());
    }
    let surviving = pids
        .iter()
        .filter(|pid| running(**pid))
        .map(u32::to_string)
        .collect::<Vec<_>>();
    anyhow::bail!(
        "process(es) {} survived SIGKILL for {}s",
        surviving.join(", "),
        grace.as_secs()
    )
}

async fn await_exit(pids: &[u32], grace: Duration) -> Result<bool> {
    let deadline = tokio::time::Instant::now() + grace;
    loop {
        if pids.iter().all(|pid| !running(*pid)) {
            return Ok(true);
        }
        if tokio::time::Instant::now() >= deadline {
            return Ok(false);
        }
        tokio::time::sleep(REAP_INTERVAL).await;
    }
}

fn signal_all(pids: &[u32], signal: libc::c_int) -> Result<()> {
    for pid in pids {
        signal_group(*pid, signal)?;
    }
    Ok(())
}

fn signal_group(pid: u32, signal: libc::c_int) -> Result<()> {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return Ok(());
    };
    // SAFETY: `kill` with a negative pid signals that process group and
    // touches no memory this process owns.
    if unsafe { libc::kill(-pid, signal) } == 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    match error.raw_os_error() {
        // Already gone, or not ours to signal - neither is a reason to fail a
        // stop that is otherwise proceeding.
        Some(libc::ESRCH | libc::EPERM) => Ok(()),
        _ => Err(error).context("failed to signal a recorded process group"),
    }
}

/// Whether the process group led by `pid` still has a member.
fn running(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    // SAFETY: signal 0 performs the permission and existence check only.
    if unsafe { libc::kill(-pid, 0) } == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runtime(id: &str, role: phoxal_cli_project::RuntimeRole) -> phoxal_cli_project::RobotRuntime {
        phoxal_cli_project::RobotRuntime {
            participant_id: id.to_string(),
            binary: id.to_string(),
            role,
            config: None,
        }
    }

    fn available() -> BTreeSet<String> {
        ["left_drive".to_string(), "right_drive".to_string()]
            .into_iter()
            .collect()
    }

    /// The launch contract is exactly four things, and `--simulation` is the
    /// only one the CLI ever decides on its own.
    #[test]
    fn a_runtimes_argv_is_the_id_the_bundle_the_endpoint_and_nothing_else() {
        let plain = argv("drive", Path::new("/srv/bundle"), "tcp/127.0.0.1:7447", false);
        assert_eq!(
            plain,
            vec![
                "--participant-id",
                "drive",
                "--bundle-root",
                "/srv/bundle",
                "--connect",
                "tcp/127.0.0.1:7447",
            ]
        );
        for retired in ["--execution-id", "--execution-origin", "--shutdown-grace-ms"] {
            assert!(!plain.iter().any(|arg| arg == retired), "{plain:?}");
        }
        let simulated = argv("drive", Path::new("/srv/bundle"), "tcp/127.0.0.1:7447", true);
        assert_eq!(simulated.last().map(String::as_str), Some("--simulation"));
    }

    /// Driver selection never touches the brain or a service: those always run,
    /// whatever the operator asked about hardware.
    #[test]
    fn a_selection_only_ever_decides_about_drivers() {
        let brain = runtime("brain", phoxal_cli_project::RuntimeRole::Brain);
        let service = runtime("drive", phoxal_cli_project::RuntimeRole::Service);
        let driver = runtime("left_drive", phoxal_cli_project::RuntimeRole::Driver);
        for selection in [
            Selection::All,
            Selection::DriversOff,
            Selection::Drivers(["left_drive".to_string()].into_iter().collect()),
        ] {
            assert!(selection.includes(&brain), "{selection:?}");
            assert!(selection.includes(&service), "{selection:?}");
        }
        assert!(Selection::All.includes(&driver));
        assert!(!Selection::DriversOff.includes(&driver));
        assert!(
            Selection::Drivers(["left_drive".to_string()].into_iter().collect()).includes(&driver)
        );
        assert!(
            !Selection::Drivers(["right_drive".to_string()].into_iter().collect())
                .includes(&driver)
        );
    }

    /// A `--driver` id the robot does not declare is a typo, and it is named
    /// against the ids that do exist rather than silently starting nothing.
    #[test]
    fn an_unknown_driver_id_is_refused_against_the_ids_the_robot_declares() {
        let error = Selection::resolve(false, vec!["left_dive".to_string()], &available())
            .expect_err("an undeclared driver id must be refused")
            .to_string();
        assert!(error.contains("left_dive"), "{error}");
        assert!(error.contains("left_drive, right_drive"), "{error}");

        assert!(Selection::resolve(true, Vec::new(), &available()).is_ok());
        assert!(
            Selection::resolve(true, vec!["left_drive".to_string()], &available()).is_err(),
            "the two flags select opposite things"
        );
    }

    /// What a selection leaves out is what the session summary has to explain.
    #[test]
    fn the_excluded_drivers_are_exactly_what_the_operator_will_see_absent() {
        assert!(Selection::All.excluded(&available()).is_empty());
        assert_eq!(
            Selection::DriversOff.excluded(&available()),
            vec!["left_drive".to_string(), "right_drive".to_string()]
        );
        assert_eq!(
            Selection::Drivers(["left_drive".to_string()].into_iter().collect())
                .excluded(&available()),
            vec!["right_drive".to_string()]
        );
    }

    fn record() -> SessionRecord {
        SessionRecord {
            project: PathBuf::from("/tmp/rover"),
            endpoint: "unixsock-stream//tmp/rover/.phoxal/run/supervisor.sock".to_string(),
            bundle: PathBuf::from("/tmp/rover/.phoxal/release/bundle"),
            simulation: false,
            supervisor: RecordedProcess {
                pid: 4_242,
                log: PathBuf::from("/tmp/rover/.phoxal/run/supervisor.log"),
            },
            runtimes: vec![RecordedRuntime {
                participant: "brain".to_string(),
                pid: 4_243,
                log: PathBuf::from("/tmp/rover/.phoxal/run/log/brain.log"),
            }],
        }
    }

    /// The record round-trips through the file `stop` reads from any terminal,
    /// under the tag serde actually writes.
    #[test]
    fn the_session_record_round_trips_through_the_file_stop_reads() -> Result<()> {
        let project = tempfile::tempdir()?;
        let paths = phoxal_cli_host::paths::RuntimePaths::for_root(project.path());
        assert!(SessionRecord::read(&paths).is_none());

        record().write(&paths)?;
        assert_eq!(SessionRecord::read(&paths).as_ref(), Some(&record()));
        let json: serde_json::Value =
            serde_json::from_slice(&std::fs::read(record_path(&paths))?)?;
        assert_eq!(json["schema"], "phoxal/cli-session/v0");
        assert_eq!(
            record().log_for("brain"),
            Some(Path::new("/tmp/rover/.phoxal/run/log/brain.log"))
        );
        assert_eq!(record().log_for("drive"), None);

        SessionRecord::remove(&paths)?;
        assert!(SessionRecord::read(&paths).is_none());
        Ok(())
    }

    /// A record this CLI cannot read is scratch bookkeeping, not an error to
    /// propagate: every later command still works, and `stop` simply finds no
    /// session to end.
    #[test]
    fn an_unreadable_record_reads_as_no_session_rather_than_as_a_failure() -> Result<()> {
        let project = tempfile::tempdir()?;
        let paths = phoxal_cli_host::paths::RuntimePaths::for_root(project.path());
        std::fs::create_dir_all(paths.volatile_root())?;
        std::fs::write(record_path(&paths), b"{\"schema\":\"phoxal/cli-session/v9\"}")?;
        assert!(SessionRecord::read(&paths).is_none());
        Ok(())
    }

    /// A stop ends a real process group and then consumes the record, so a
    /// second stop has nothing to end.
    #[tokio::test]
    async fn stopping_a_session_ends_its_processes_and_consumes_the_record() -> Result<()> {
        let project = tempfile::tempdir()?;
        let paths = phoxal_cli_host::paths::RuntimePaths::for_root(project.path());
        let mut sleeper = {
            use std::os::unix::process::CommandExt;
            let mut command = Command::new("sleep");
            command.arg("120").stdin(Stdio::null());
            command.process_group(0);
            command.spawn()?
        };
        let pid = sleeper.id();
        assert!(running(pid));

        let mut record = record();
        record.supervisor.pid = pid;
        record.runtimes.clear();
        record.write(&paths)?;

        stop_session(&record, &paths).await?;
        // Reap the child this test owns so the assertion below reads the
        // kernel's answer rather than a zombie's lingering entry.
        let _ = sleeper.wait();
        assert!(!running(pid));
        assert!(SessionRecord::read(&paths).is_none());
        Ok(())
    }
}
