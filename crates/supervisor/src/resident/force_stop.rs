//! Protocol-independent resident termination.

use std::fmt;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use phoxal_cli_core::runtime::{ResidentAuthority, RuntimeTarget};

use crate::{ProjectLock, ProjectLockStatus, ProjectOperation};

pub const DETACHED_FORCE_STOP_GRACE: Duration = Duration::from_secs(30);
pub const SYSTEMD_FORCE_STOP_GRACE: Duration = Duration::from_secs(300);
const ESCALATION_WAIT: Duration = Duration::from_secs(30);
const POLL_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForceStopOutcome {
    Graceful,
    Forced,
}

impl fmt::Display for ForceStopOutcome {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Graceful => "graceful",
            Self::Forced => "forced",
        })
    }
}

pub async fn force_stop(target: &RuntimeTarget) -> Result<ForceStopOutcome> {
    let target = target.clone();
    tokio::task::spawn_blocking(move || force_stop_with(&target, &RealAuthority, durations()))
        .await
        .context("force-stop worker panicked")?
}

#[derive(Clone, Copy)]
struct Durations {
    detached_grace: Duration,
    systemd_grace: Duration,
    escalation_wait: Duration,
    poll: Duration,
}

const fn durations() -> Durations {
    Durations {
        detached_grace: DETACHED_FORCE_STOP_GRACE,
        systemd_grace: SYSTEMD_FORCE_STOP_GRACE,
        escalation_wait: ESCALATION_WAIT,
        poll: POLL_INTERVAL,
    }
}

trait Authority {
    fn inspect_lock(&self, path: &Path) -> Result<ProjectLockStatus>;
    fn validate_session_leader(&self, pid: u32) -> Result<()>;
    fn signal_group(&self, pid: u32, signal: i32) -> Result<()>;
    fn group_exists(&self, pid: u32) -> bool;
    fn systemctl(&self, args: &[&str]) -> Result<()>;
    fn unit_active(&self, unit: &str) -> Result<bool>;
    fn sleep(&self, duration: Duration);
}

fn force_stop_with(
    target: &RuntimeTarget,
    authority: &impl Authority,
    durations: Durations,
) -> Result<ForceStopOutcome> {
    let identity = match authority.inspect_lock(&target.project_lock)? {
        ProjectLockStatus::Free => {
            bail!("project is not running: {}", target.logical_root.display())
        }
        ProjectLockStatus::Held(identity) if identity.operation != ProjectOperation::Run => {
            bail!(
                "project is held by `{}` (pid {}), not a resident run",
                identity.operation,
                identity.pid
            )
        }
        ProjectLockStatus::Held(identity) => identity,
    };
    if let Some(requested_entry) = &target.requested_entry {
        let requested = requested_entry
            .canonicalize()
            .unwrap_or_else(|_| requested_entry.clone());
        let running = identity
            .entry
            .canonicalize()
            .unwrap_or_else(|_| identity.entry.clone());
        anyhow::ensure!(
            requested == running,
            "entry mismatch: requested {}, but the running entry is {}",
            requested.display(),
            running.display()
        );
    }

    match &target.authority {
        ResidentAuthority::DetachedSession => {
            anyhow::ensure!(
                identity.pid > 1,
                "refusing to signal resident pid {}: negative pid signalling is unsafe for pid 1",
                identity.pid
            );
            authority.validate_session_leader(identity.pid)?;
            authority.signal_group(identity.pid, libc::SIGTERM)?;
            if wait_until(durations.detached_grace, durations.poll, authority, || {
                stopped_detached(authority, &target.project_lock, identity.pid)
            })? {
                return Ok(ForceStopOutcome::Graceful);
            }
            authority.signal_group(identity.pid, libc::SIGKILL)?;
            anyhow::ensure!(
                wait_until(durations.escalation_wait, durations.poll, authority, || {
                    stopped_detached(authority, &target.project_lock, identity.pid)
                },)?,
                "resident process group {} remained after SIGKILL",
                identity.pid
            );
            Ok(ForceStopOutcome::Forced)
        }
        ResidentAuthority::SystemdUnit { unit } => {
            run_systemctl_action(authority, &["stop", unit], "stop", unit)?;
            if wait_until(durations.systemd_grace, durations.poll, authority, || {
                stopped_systemd(authority, &target.project_lock, unit)
            })? {
                return Ok(ForceStopOutcome::Graceful);
            }
            run_systemctl_action(
                authority,
                &["kill", "--kill-who=all", "--signal=SIGKILL", unit.as_str()],
                "kill --kill-who=all --signal=SIGKILL",
                unit,
            )?;
            anyhow::ensure!(
                wait_until(durations.escalation_wait, durations.poll, authority, || {
                    stopped_systemd(authority, &target.project_lock, unit)
                },)?,
                "systemd unit {unit} remained active or retained its project lock after forced termination"
            );
            Ok(ForceStopOutcome::Forced)
        }
    }
}

fn stopped_detached(authority: &impl Authority, lock: &Path, pid: u32) -> Result<bool> {
    Ok(
        matches!(authority.inspect_lock(lock)?, ProjectLockStatus::Free)
            && !authority.group_exists(pid),
    )
}

fn stopped_systemd(authority: &impl Authority, lock: &Path, unit: &str) -> Result<bool> {
    Ok(
        matches!(authority.inspect_lock(lock)?, ProjectLockStatus::Free)
            && !authority.unit_active(unit)?,
    )
}

fn wait_until(
    timeout: Duration,
    poll: Duration,
    authority: &impl Authority,
    mut condition: impl FnMut() -> Result<bool>,
) -> Result<bool> {
    let deadline = Instant::now() + timeout;
    loop {
        if condition()? {
            return Ok(true);
        }
        if Instant::now() >= deadline {
            return Ok(false);
        }
        authority.sleep(poll);
    }
}

fn run_systemctl_action(
    authority: &impl Authority,
    args: &[&str],
    action: &str,
    unit: &str,
) -> Result<()> {
    authority.systemctl(args).with_context(|| {
        format!(
            "failed to stop the installed resident; run with sufficient systemd/polkit authority:\n\n    systemctl {action} {unit}"
        )
    })
}

struct RealAuthority;

impl Authority for RealAuthority {
    fn inspect_lock(&self, path: &Path) -> Result<ProjectLockStatus> {
        ProjectLock::inspect_path(path)
    }

    fn validate_session_leader(&self, pid: u32) -> Result<()> {
        let pid = i32::try_from(pid).context("resident pid exceeds platform pid range")?;
        // SAFETY: both calls inspect one numeric process identity and do not
        // retain pointers or borrowed state.
        let group = unsafe { libc::getpgid(pid) };
        let session = unsafe { libc::getsid(pid) };
        anyhow::ensure!(
            group == pid && session == pid,
            "refusing to signal pid {pid}: it is not both process-group and session leader (pgid={group}, sid={session})"
        );
        Ok(())
    }

    fn signal_group(&self, pid: u32, signal: i32) -> Result<()> {
        let pid = i32::try_from(pid).context("resident pid exceeds platform pid range")?;
        // SAFETY: a negative, previously validated process-group id targets
        // only that resident session.
        let result = unsafe { libc::kill(-pid, signal) };
        if result == -1 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("signal resident process group {pid}"));
        }
        Ok(())
    }

    fn group_exists(&self, pid: u32) -> bool {
        let Ok(pid) = i32::try_from(pid) else {
            return false;
        };
        // SAFETY: signal zero performs existence/permission checking only.
        (unsafe { libc::kill(-pid, 0) == 0 })
            || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
    }

    fn systemctl(&self, args: &[&str]) -> Result<()> {
        let status = Command::new("systemctl").args(args).status()?;
        anyhow::ensure!(status.success(), "systemctl exited with {status}");
        Ok(())
    }

    fn unit_active(&self, unit: &str) -> Result<bool> {
        let status = Command::new("systemctl")
            .args(["is-active", "--quiet", unit])
            .status()?;
        Ok(status.success())
    }

    fn sleep(&self, duration: Duration) {
        std::thread::sleep(duration);
    }
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::path::PathBuf;

    use phoxal_cli_core::runtime::ResidentAuthority;

    use super::*;
    use crate::ProjectLockIdentity;

    struct FakeAuthority {
        identity: ProjectLockIdentity,
        stopped: Cell<bool>,
        unit_active: Cell<bool>,
        term_stops: bool,
        systemd_stop_stops: bool,
        signals: RefCell<Vec<i32>>,
        systemctl_calls: RefCell<Vec<Vec<String>>>,
    }

    impl FakeAuthority {
        fn new(term_stops: bool, systemd_stop_stops: bool) -> Self {
            Self {
                identity: ProjectLockIdentity {
                    project: PathBuf::from("/tmp/project"),
                    entry: PathBuf::from("/tmp/project/robot.yaml"),
                    operation: ProjectOperation::Run,
                    pid: 42,
                    execution: None,
                },
                stopped: Cell::new(false),
                unit_active: Cell::new(true),
                term_stops,
                systemd_stop_stops,
                signals: RefCell::new(Vec::new()),
                systemctl_calls: RefCell::new(Vec::new()),
            }
        }
    }

    impl Authority for FakeAuthority {
        fn inspect_lock(&self, _path: &Path) -> Result<ProjectLockStatus> {
            Ok(if self.stopped.get() {
                ProjectLockStatus::Free
            } else {
                ProjectLockStatus::Held(self.identity.clone())
            })
        }

        fn validate_session_leader(&self, _pid: u32) -> Result<()> {
            Ok(())
        }

        fn signal_group(&self, _pid: u32, signal: i32) -> Result<()> {
            self.signals.borrow_mut().push(signal);
            if signal == libc::SIGKILL || (signal == libc::SIGTERM && self.term_stops) {
                self.stopped.set(true);
            }
            Ok(())
        }

        fn group_exists(&self, _pid: u32) -> bool {
            !self.stopped.get()
        }

        fn systemctl(&self, args: &[&str]) -> Result<()> {
            self.systemctl_calls
                .borrow_mut()
                .push(args.iter().map(|value| (*value).to_string()).collect());
            if args.first() == Some(&"stop") && self.systemd_stop_stops
                || args.first() == Some(&"kill")
            {
                self.stopped.set(true);
                self.unit_active.set(false);
            }
            Ok(())
        }

        fn unit_active(&self, _unit: &str) -> Result<bool> {
            Ok(self.unit_active.get())
        }

        fn sleep(&self, _duration: Duration) {}
    }

    fn target(authority: ResidentAuthority) -> RuntimeTarget {
        RuntimeTarget {
            logical_root: PathBuf::from("/tmp/project"),
            requested_entry: None,
            project_lock: PathBuf::from("/tmp/project/.phoxal/project.lock"),
            supervisor_socket: PathBuf::from("/tmp/project/.phoxal/supervisor.sock"),
            zenoh_socket: PathBuf::from("/tmp/project/.phoxal/zenoh.sock"),
            zenoh_endpoint: "unixsock-stream//tmp/project/.phoxal/zenoh.sock".to_string(),
            authority,
        }
    }

    const TEST_DURATIONS: Durations = Durations {
        detached_grace: Duration::ZERO,
        systemd_grace: Duration::ZERO,
        escalation_wait: Duration::ZERO,
        poll: Duration::ZERO,
    };

    #[test]
    fn detached_force_stop_reports_graceful_and_forced_paths() {
        let graceful = FakeAuthority::new(true, false);
        assert_eq!(
            force_stop_with(
                &target(ResidentAuthority::DetachedSession),
                &graceful,
                TEST_DURATIONS
            )
            .unwrap(),
            ForceStopOutcome::Graceful
        );
        assert_eq!(*graceful.signals.borrow(), vec![libc::SIGTERM]);

        let forced = FakeAuthority::new(false, false);
        assert_eq!(
            force_stop_with(
                &target(ResidentAuthority::DetachedSession),
                &forced,
                TEST_DURATIONS
            )
            .unwrap(),
            ForceStopOutcome::Forced
        );
        assert_eq!(*forced.signals.borrow(), vec![libc::SIGTERM, libc::SIGKILL]);
    }

    #[test]
    fn systemd_force_stop_selects_stop_then_kill_when_needed() {
        let graceful = FakeAuthority::new(false, true);
        let systemd_target = target(ResidentAuthority::SystemdUnit {
            unit: "phoxal.service".to_string(),
        });
        assert_eq!(
            force_stop_with(&systemd_target, &graceful, TEST_DURATIONS).unwrap(),
            ForceStopOutcome::Graceful
        );
        assert_eq!(
            *graceful.systemctl_calls.borrow(),
            vec![vec!["stop".to_string(), "phoxal.service".to_string()]]
        );

        let forced = FakeAuthority::new(false, false);
        assert_eq!(
            force_stop_with(&systemd_target, &forced, TEST_DURATIONS).unwrap(),
            ForceStopOutcome::Forced
        );
        assert_eq!(
            *forced.systemctl_calls.borrow(),
            vec![
                vec!["stop".to_string(), "phoxal.service".to_string()],
                vec![
                    "kill".to_string(),
                    "--kill-who=all".to_string(),
                    "--signal=SIGKILL".to_string(),
                    "phoxal.service".to_string(),
                ],
            ]
        );
    }

    #[test]
    fn force_stop_refuses_free_non_run_pid_one_and_entry_mismatch() {
        let detached = target(ResidentAuthority::DetachedSession);

        let free = FakeAuthority::new(false, false);
        free.stopped.set(true);
        assert!(
            force_stop_with(&detached, &free, TEST_DURATIONS)
                .unwrap_err()
                .to_string()
                .contains("project is not running")
        );

        let mut non_run = FakeAuthority::new(false, false);
        non_run.identity.operation = ProjectOperation::Build;
        assert!(
            force_stop_with(&detached, &non_run, TEST_DURATIONS)
                .unwrap_err()
                .to_string()
                .contains("not a resident run")
        );

        let mut pid_one = FakeAuthority::new(false, false);
        pid_one.identity.pid = 1;
        assert!(
            force_stop_with(&detached, &pid_one, TEST_DURATIONS)
                .unwrap_err()
                .to_string()
                .contains("unsafe for pid 1")
        );
        assert!(pid_one.signals.borrow().is_empty());

        let mut wrong_entry = detached;
        wrong_entry.requested_entry = Some(PathBuf::from("/tmp/project/other.yaml"));
        let authority = FakeAuthority::new(false, false);
        assert!(
            force_stop_with(&wrong_entry, &authority, TEST_DURATIONS)
                .unwrap_err()
                .to_string()
                .contains("entry mismatch")
        );
    }
}
