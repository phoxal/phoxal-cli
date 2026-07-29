use std::ffi::OsStr;
use std::io::{BufRead, Read};
use std::path::Path;
use std::process::{Command, ExitStatus, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::{PreparationEvent, Reporter};

const SUDO_PASSWORD_ENV: &str = "PHOXAL_SUDO_PASSWORD";

#[derive(Clone, Copy, PartialEq, Eq)]
enum CapturedStream {
    Stdout,
    Stderr,
}

pub(crate) fn command_status_captured(
    command: &mut Command,
    reporter: &dyn Reporter,
) -> Result<ExitStatus> {
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let child = Arc::new(Mutex::new(Some(
        command.spawn().context("failed to spawn command")?,
    )));
    let pipes = {
        let mut child = child
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        child
            .as_mut()
            .context("captured child was already reaped")
            .and_then(|child| {
                Ok((
                    child.stdout.take().context("child stdout was not piped")?,
                    child.stderr.take().context("child stderr was not piped")?,
                ))
            })
    };
    let (stdout, stderr) = pipes?;
    let status = std::thread::scope(|scope| {
        let stdout_task = scope.spawn(|| forward_lines(stdout, CapturedStream::Stdout, reporter));
        let stderr_task = scope.spawn(|| forward_lines(stderr, CapturedStream::Stderr, reporter));
        let status = wait_for_child(&child, Some(reporter));
        let stdout_result = stdout_task.join();
        let stderr_result = stderr_task.join();
        (status, stdout_result, stderr_result)
    });
    let (status, stdout, stderr) = status;
    let status = status?;
    stdout.map_err(|_| anyhow::anyhow!("stdout reader thread panicked"))??;
    let stderr = stderr.map_err(|_| anyhow::anyhow!("stderr reader thread panicked"))??;
    if !status.success() {
        for line in stderr {
            reporter.error(line);
        }
    }
    Ok(status)
}

fn forward_lines(
    reader: impl Read,
    stream: CapturedStream,
    reporter: &dyn Reporter,
) -> Result<Vec<String>> {
    const MAX_CAPTURED_ERROR_LINES: usize = 200;
    let mut captured = std::collections::VecDeque::new();
    for line in std::io::BufReader::new(reader).lines() {
        let line = line.context("failed to read captured command output")?;
        if line.is_empty() {
            continue;
        }
        reporter.report(PreparationEvent::CommandLine(line.clone()));
        if stream == CapturedStream::Stderr {
            if captured.len() == MAX_CAPTURED_ERROR_LINES {
                captured.pop_front();
            }
            captured.push_back(line);
        }
    }
    Ok(captured.into())
}

pub fn run_output(
    executable: &str,
    args: impl IntoIterator<Item = impl AsRef<OsStr>>,
    cwd: Option<&Path>,
    reporter: Option<&dyn Reporter>,
) -> Result<Output> {
    let mut command = Command::new(executable);
    command.env_remove(SUDO_PASSWORD_ENV);
    command.args(args);
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let child = Arc::new(Mutex::new(Some(
        command
            .spawn()
            .with_context(|| format!("failed to run `{executable}`"))?,
    )));
    let pipes = {
        let mut child = child
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        child
            .as_mut()
            .context("captured child was already reaped")
            .and_then(|child| {
                Ok((
                    child.stdout.take().context("child stdout was not piped")?,
                    child.stderr.take().context("child stderr was not piped")?,
                ))
            })
    };
    let (stdout, stderr) = pipes?;
    let stdout_task = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let result = std::io::BufReader::new(stdout).read_to_end(&mut bytes);
        (result, bytes)
    });
    let stderr_task = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let result = std::io::BufReader::new(stderr).read_to_end(&mut bytes);
        (result, bytes)
    });
    let status = wait_for_child(&child, reporter);
    let status = status?;
    let (stdout_result, stdout) = stdout_task
        .join()
        .map_err(|_| anyhow::anyhow!("stdout reader thread panicked"))?;
    stdout_result.context("failed to read child stdout")?;
    let (stderr_result, stderr) = stderr_task
        .join()
        .map_err(|_| anyhow::anyhow!("stderr reader thread panicked"))?;
    stderr_result.context("failed to read child stderr")?;
    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

fn wait_for_child(
    child: &Arc<Mutex<Option<std::process::Child>>>,
    reporter: Option<&dyn Reporter>,
) -> Result<ExitStatus> {
    loop {
        {
            let mut child = child
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let running = child
                .as_mut()
                .context("captured child was already reaped")?;
            if reporter.is_some_and(Reporter::is_cancelled) {
                terminate_process_group(running);
            }
            if let Some(status) = running.try_wait().context("failed to wait for command")? {
                child.take();
                return Ok(status);
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn terminate_process_group(child: &mut std::process::Child) {
    #[cfg(unix)]
    if let Ok(group) = i32::try_from(child.id()) {
        // SAFETY: every captured command is created as its own process-group
        // leader above, so the negative pid targets only that command tree.
        let _ = unsafe { libc::kill(-group, libc::SIGKILL) };
    }
    #[cfg(not(unix))]
    let _ = child.kill();
}

pub fn run_stdout(
    executable: &str,
    args: impl IntoIterator<Item = impl AsRef<OsStr>>,
    cwd: Option<&Path>,
    reporter: Option<&dyn Reporter>,
) -> Result<String> {
    let output = run_output(executable, args, cwd, reporter)?;
    if output.status.success() {
        return String::from_utf8(output.stdout)
            .with_context(|| format!("`{}` wrote non-UTF8 stdout", executable));
    }
    bail!(
        "`{}` failed with status {}\nstdout:\n{}\nstderr:\n{}",
        executable,
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Instant;

    #[derive(Default)]
    struct RecordingReporter {
        events: Mutex<Vec<PreparationEvent>>,
        cancelled: AtomicBool,
    }

    impl Reporter for RecordingReporter {
        fn report(&self, event: PreparationEvent) {
            self.events
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(event);
        }

        fn is_cancelled(&self) -> bool {
            self.cancelled.load(Ordering::Acquire)
        }
    }

    #[cfg(unix)]
    #[test]
    fn captured_failure_replays_only_the_bounded_stderr_tail() -> Result<()> {
        let reporter = RecordingReporter::default();
        let mut command = Command::new("/bin/sh");
        command.args([
            "-c",
            "i=1; while [ \"$i\" -le 250 ]; do echo \"failure-$i\" >&2; i=$((i+1)); done; exit 7",
        ]);
        let status = command_status_captured(&mut command, &reporter)?;
        assert_eq!(status.code(), Some(7));

        let events = reporter
            .events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let errors = events
            .iter()
            .filter_map(|event| match event {
                PreparationEvent::Error(line) => Some(line.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(errors.len(), 200);
        assert_eq!(errors.first().copied(), Some("failure-51"));
        assert_eq!(errors.last().copied(), Some("failure-250"));
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn cancellation_terminates_the_captured_command_process_group() -> Result<()> {
        let reporter = Arc::new(RecordingReporter::default());
        let canceller = Arc::clone(&reporter);
        let cancel = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            canceller.cancelled.store(true, Ordering::Release);
        });
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "sleep 30 & wait"]);
        let started = Instant::now();
        let status = command_status_captured(&mut command, reporter.as_ref())?;
        cancel.join().expect("cancellation thread");
        assert!(!status.success());
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "cancelled command tree was not terminated promptly"
        );
        Ok(())
    }
}
