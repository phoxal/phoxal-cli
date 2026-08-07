//! Launching `phoxald` and watching it until a client can attach.
//!
//! The daemon is a sibling executable, never a mode of this one: `phoxal` can
//! never run the supervision loop in process. Everything
//! this module knows about the child is process facts and stderr - the
//! completed Zenoh handshake is readiness, and a socket file's existence
//! proves nothing.

use std::io::Read;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};

/// How much of the daemon's stderr is kept as early-exit evidence. The daemon
/// writes its own log; this is only what a client shows when the child died
/// before there was anything to attach to.
const MAX_DIAGNOSTIC_BYTES: usize = 8 * 1024;

/// A launched daemon and the stderr this client is capturing from it.
pub(crate) struct LaunchedDaemon {
    child: Child,
    stderr: Arc<Mutex<String>>,
    /// The stderr pump. It ends when the pipe closes, which is when the child
    /// exits, so it is never joined - holding it keeps the handle owned rather
    /// than detached.
    _reader: Option<std::thread::JoinHandle<()>>,
}

impl LaunchedDaemon {
    /// The daemon's stderr so far, bounded.
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
    /// daemon managed to say before it went.
    pub(crate) fn early_exit_message(&self, status: std::process::ExitStatus) -> String {
        let diagnostics = self.diagnostics();
        let diagnostics = diagnostics.trim();
        if diagnostics.is_empty() {
            format!("phoxald exited with {status} before a client could attach")
        } else {
            format!("phoxald exited with {status} before a client could attach: {diagnostics}")
        }
    }
}

/// Spawn `phoxald <BUNDLE_ROOT>`.
///
/// The daemon is placed in its own process group. That is not tidiness: a
/// Ctrl+C in the terminal running this client goes to the client's foreground
/// process group, and the daemon is durable - it must survive the client that
/// started it, because detaching is not stopping.
pub(crate) fn spawn(bundle_root: &Path) -> Result<LaunchedDaemon> {
    // The pair is confirmed before anything is built, so by here the sibling is
    // known to exist and to be this client's exact version.
    let executable = crate::pair::resolve_daemon();
    let mut command = Command::new(&executable);
    command
        .arg(bundle_root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    isolate_process_group(&mut command);
    let mut child = command.spawn().with_context(|| {
        format!(
            "failed to launch the supervisor {} on {}",
            executable.display(),
            bundle_root.display()
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

    Ok(LaunchedDaemon {
        child,
        stderr,
        _reader: reader,
    })
}

#[cfg(unix)]
fn isolate_process_group(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    // `setpgid(0, 0)` in the child: it leaves this client's foreground process
    // group, so a terminal SIGINT never reaches it.
    command.process_group(0);
}

#[cfg(not(unix))]
fn isolate_process_group(_command: &mut Command) {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pair::DAEMON_BINARY;

    /// The daemon must not be in this client's process group, or a terminal
    /// Ctrl+C would stop the robot the client only meant to detach from
    ///.
    #[test]
    fn a_launched_daemon_leaves_this_clients_process_group() {
        let script = tempfile::tempdir().expect("temp dir");
        let executable = script.path().join(DAEMON_BINARY);
        std::fs::write(
            &executable,
            "#!/bin/sh\nps -o pgid= -p $$ | tr -d ' ' >&2\nexit 3\n",
        )
        .expect("write fixture daemon");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755))
                .expect("make the fixture executable");
        }

        let mut command = Command::new(&executable);
        command.stderr(Stdio::piped()).stdout(Stdio::null());
        isolate_process_group(&mut command);
        let output = command.output().expect("run the fixture daemon");
        let child_group: i32 = String::from_utf8_lossy(&output.stderr)
            .trim()
            .parse()
            .expect("the fixture reports its process group");

        // SAFETY: `getpgrp` reads this process's own group and takes no
        // pointer.
        let own_group = unsafe { libc::getpgrp() };
        assert_ne!(
            child_group, own_group,
            "the daemon must not share the client's process group"
        );
    }

    /// An early exit is diagnosable: the client reports what the daemon said,
    /// not merely that something went wrong.
    #[test]
    fn an_early_exit_carries_the_daemons_own_stderr() {
        let script = tempfile::tempdir().expect("temp dir");
        let executable = script.path().join(DAEMON_BINARY);
        std::fs::write(
            &executable,
            "#!/bin/sh\necho 'phoxald: not a finalized bundle' >&2\nexit 1\n",
        )
        .expect("write fixture daemon");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755))
                .expect("make the fixture executable");
        }
        let mut command = Command::new(&executable);
        command.stderr(Stdio::piped()).stdout(Stdio::null());
        let mut child = command.spawn().expect("spawn the fixture daemon");
        let stderr = Arc::new(Mutex::new(String::new()));
        {
            let mut pipe = child.stderr.take().expect("piped stderr");
            let mut buffer = String::new();
            pipe.read_to_string(&mut buffer).expect("read stderr");
            *stderr.lock().unwrap() = buffer;
        }
        let launched = LaunchedDaemon {
            child,
            stderr,
            _reader: None,
        };
        let (status, _) = {
            let mut launched = launched;
            let status = launched.child.wait().expect("wait");
            let message = launched.early_exit_message(status);
            assert!(message.contains("not a finalized bundle"), "{message}");
            assert!(
                message.contains("before a client could attach"),
                "{message}"
            );
            (status, message)
        };
        assert!(!status.success());
    }
}
