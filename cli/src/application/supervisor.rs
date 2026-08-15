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

use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};

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
