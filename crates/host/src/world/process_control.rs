//! Process identity inspection and validated native process-group control.

use super::*;

/// Supplies process start time so PID-reuse behavior is deterministic in tests.
pub trait ProcessInspector {
    fn started_at_unix_s(&self, pid: u32) -> Option<u64>;
}

/// One fresh process-table observation used immediately before a native
/// process-group signal.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ObservedNativeProcess {
    pub process: ProcessIdentity,
    pub executable: PathBuf,
    pub process_group: u32,
}

/// Narrow process control boundary for deterministic orphan-recovery tests.
pub trait NativeProcessControl {
    fn observe(&self, pid: u32) -> Result<Option<ObservedNativeProcess>>;
    fn process_group_alive(&self, process_group: u32) -> Result<bool>;
    fn signal_process_group(&self, process_group: u32, signal: NativeSignal) -> Result<()>;
    fn wait(&self, duration: Duration);
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeSignal {
    Terminate,
    Kill,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SystemProcessInspector;

impl ProcessInspector for SystemProcessInspector {
    fn started_at_unix_s(&self, pid: u32) -> Option<u64> {
        let pid = Pid::from_u32(pid);
        let mut system = System::new();
        system.refresh_processes(ProcessesToUpdate::Some(&[pid]), true);
        system.process(pid).map(sysinfo::Process::start_time)
    }
}

impl NativeProcessControl for SystemProcessInspector {
    fn observe(&self, pid: u32) -> Result<Option<ObservedNativeProcess>> {
        let process_id = Pid::from_u32(pid);
        let mut system = System::new();
        system.refresh_processes(ProcessesToUpdate::Some(&[process_id]), true);
        let Some(process) = system.process(process_id) else {
            return Ok(None);
        };
        let Some(executable) = process.exe() else {
            bail!("process {pid} has no observable executable path");
        };
        let native_pid = libc::pid_t::try_from(pid).context("native PID does not fit pid_t")?;
        // SAFETY: `getpgid` takes a scalar PID and does not dereference memory.
        let process_group = unsafe { libc::getpgid(native_pid) };
        if process_group < 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ESRCH) {
                return Ok(None);
            }
            return Err(error).context("failed to inspect the native process group");
        }
        Ok(Some(ObservedNativeProcess {
            process: ProcessIdentity {
                pid,
                started_at_unix_s: process.start_time(),
            },
            executable: executable.to_path_buf(),
            process_group: u32::try_from(process_group)
                .context("native process group is negative")?,
        }))
    }

    fn process_group_alive(&self, process_group: u32) -> Result<bool> {
        probe_process_group(process_group)
    }

    fn signal_process_group(&self, process_group: u32, signal: NativeSignal) -> Result<()> {
        signal_owned_process_group(process_group, signal)
    }

    fn wait(&self, duration: Duration) {
        std::thread::sleep(duration);
    }
}
pub(super) fn converge_native_process_group<C: NativeProcessControl>(
    expected: &NativeProcessIdentity,
    control: &C,
) -> Result<()> {
    let process_group = expected
        .process_group
        .context("native checkpoint has no Unix process-group ownership")?;
    ensure!(
        process_group == expected.process.pid,
        "native checkpoint process group does not equal its direct process PID"
    );

    control.wait(NATIVE_EXIT_GRACE);
    if !control.process_group_alive(process_group)? {
        return Ok(());
    }

    for (signal, budget) in [
        (NativeSignal::Terminate, NATIVE_TERM_BUDGET),
        (NativeSignal::Kill, NATIVE_KILL_BUDGET),
    ] {
        validate_native_process_before_signal(expected, control)?;
        control.signal_process_group(process_group, signal)?;
        let mut remaining = budget;
        while !remaining.is_zero() {
            if !control.process_group_alive(process_group)? {
                return Ok(());
            }
            let interval = NATIVE_POLL_INTERVAL.min(remaining);
            control.wait(interval);
            remaining = remaining.saturating_sub(interval);
        }
        if !control.process_group_alive(process_group)? {
            return Ok(());
        }
    }
    bail!("orphaned native process group {process_group} remained alive after SIGKILL")
}

fn validate_native_process_before_signal<C: NativeProcessControl>(
    expected: &NativeProcessIdentity,
    control: &C,
) -> Result<()> {
    let canonical_executable = expected.executable.canonicalize().with_context(|| {
        format!(
            "failed to canonicalize checkpointed native executable {}",
            expected.executable.display()
        )
    })?;
    ensure!(
        canonical_executable == expected.executable,
        "checkpointed native executable {} is not canonical; refusing to signal its process group",
        expected.executable.display()
    );
    let observed = control.observe(expected.process.pid)?.with_context(|| {
        format!(
            "native process group {} is live but its direct process {} is absent; refusing an unvalidated group signal",
            expected.process_group.unwrap_or_default(),
            expected.process.pid
        )
    })?;
    ensure!(
        observed.process == expected.process,
        "native PID {} was reused or its birth identity changed; refusing to signal process group {}",
        expected.process.pid,
        observed.process_group
    );
    ensure!(
        observed.executable == canonical_executable,
        "native PID {} executable is {}, expected {}; refusing to signal an ambiguous process group",
        expected.process.pid,
        observed.executable.display(),
        expected.executable.display()
    );
    ensure!(
        Some(observed.process_group) == expected.process_group,
        "native PID {} belongs to process group {}, expected {}; refusing to signal it",
        expected.process.pid,
        observed.process_group,
        expected.process_group.unwrap_or_default()
    );
    Ok(())
}

#[cfg(unix)]
fn probe_process_group(process_group: u32) -> Result<bool> {
    let process_group = libc::pid_t::try_from(process_group)
        .context("native process-group ID does not fit pid_t")?;
    ensure!(
        process_group > 0,
        "native process-group ID must be positive"
    );
    // SAFETY: `kill` takes no pointer. A negative PID and signal zero probe
    // the exact process group without delivering a signal.
    if unsafe { libc::kill(-process_group, 0) } == 0 {
        return Ok(true);
    }
    let error = std::io::Error::last_os_error();
    match error.raw_os_error() {
        Some(libc::ESRCH) => Ok(false),
        Some(libc::EPERM) => Ok(true),
        _ => Err(error).context("failed to inspect the native process group"),
    }
}

#[cfg(not(unix))]
fn probe_process_group(_process_group: u32) -> Result<bool> {
    bail!("local world orphan recovery requires Unix process-group semantics")
}

#[cfg(unix)]
fn signal_owned_process_group(process_group: u32, signal: NativeSignal) -> Result<()> {
    let process_group = libc::pid_t::try_from(process_group)
        .context("native process-group ID does not fit pid_t")?;
    ensure!(
        process_group > 0,
        "native process-group ID must be positive"
    );
    let signal = match signal {
        NativeSignal::Terminate => libc::SIGTERM,
        NativeSignal::Kill => libc::SIGKILL,
    };
    // SAFETY: `kill` takes no pointer. The negative PID targets only the
    // checkpoint-owned process group that was just revalidated.
    if unsafe { libc::kill(-process_group, signal) } == 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        return Ok(());
    }
    Err(error).context("failed to signal the orphaned native process group")
}

#[cfg(not(unix))]
fn signal_owned_process_group(_process_group: u32, _signal: NativeSignal) -> Result<()> {
    bail!("local world orphan recovery requires Unix process-group semantics")
}
