use std::ffi::OsStr;
use std::path::Path;
use std::process::{Command, Output};

use anyhow::{Context, Result, bail};

pub fn run_output(
    executable: &str,
    args: impl IntoIterator<Item = impl AsRef<OsStr>>,
    cwd: Option<&Path>,
) -> Result<Output> {
    let mut command = Command::new(executable);
    command.args(args);
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    command
        .output()
        .with_context(|| format!("failed to run `{}`", executable))
}

pub fn run_status(
    executable: &str,
    args: impl IntoIterator<Item = impl AsRef<OsStr>>,
    cwd: Option<&Path>,
) -> Result<()> {
    let output = run_output(executable, args, cwd)?;
    if output.status.success() {
        return Ok(());
    }
    bail!(
        "`{}` failed with status {}\nstdout:\n{}\nstderr:\n{}",
        executable,
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
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
