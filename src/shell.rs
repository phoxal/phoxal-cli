use std::ffi::OsStr;
use std::io::Read;
use std::path::Path;
use std::process::{Command, ExitStatus, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::session::diagnostics::{register_child, unregister_child};

const SUDO_PASSWORD_ENV: &str = "PHOXAL_SUDO_PASSWORD";

pub fn run_output(
    executable: &str,
    args: impl IntoIterator<Item = impl AsRef<OsStr>>,
    cwd: Option<&Path>,
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
    register_child(child.clone());
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
    let (stdout, stderr) = match pipes {
        Ok(pipes) => pipes,
        Err(error) => {
            unregister_child(&child);
            return Err(error);
        }
    };
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
    let status = wait_for_registered_child(&child);
    unregister_child(&child);
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

fn wait_for_registered_child(
    child: &Arc<Mutex<Option<std::process::Child>>>,
) -> Result<ExitStatus> {
    loop {
        {
            let mut child = child
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let running = child
                .as_mut()
                .context("captured child was already reaped")?;
            if let Some(status) = running.try_wait().context("failed to wait for command")? {
                child.take();
                return Ok(status);
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

pub fn run_stdout(
    executable: &str,
    args: impl IntoIterator<Item = impl AsRef<OsStr>>,
    cwd: Option<&Path>,
) -> Result<String> {
    let output = run_output(executable, args, cwd)?;
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
