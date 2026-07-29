use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use phoxal_cli_core::identity::ExecutionId;
use phoxal_cli_protocol::BootstrapResult;
use phoxal_cli_protocol::codec::blocking_io;
use phoxal_cli_protocol::limits::MAX_HANDSHAKE_FRAME_BYTES;

use super::bootstrap::{BOOTSTRAP_EXECUTION_ENV, BOOTSTRAP_FD_ENV};
use super::log::resident_log_file;

pub struct LaunchedResident {
    pub child: Child,
    pub result: BootstrapResult,
}

#[must_use]
pub fn has_private_bootstrap() -> bool {
    std::env::var_os(BOOTSTRAP_FD_ENV).is_some()
}

pub fn launch_detached(offline: bool) -> Result<LaunchedResident> {
    let (mut parent, child_socket) =
        std::os::unix::net::UnixStream::pair().context("create resident bootstrap socketpair")?;
    parent.set_read_timeout(Some(Duration::from_secs(30)))?;
    parent.set_write_timeout(Some(Duration::from_secs(30)))?;
    child_socket.set_read_timeout(Some(Duration::from_secs(30)))?;
    child_socket.set_write_timeout(Some(Duration::from_secs(30)))?;
    let child_fd = child_socket.as_raw_fd();
    let execution = ExecutionId::mint();
    let log = resident_log_file()?;
    let stderr = log.try_clone().context("clone resident log descriptor")?;
    let mut command = Command::new(std::env::current_exe()?);
    command
        .args(detached_command_args(std::env::args_os().skip(1), offline))
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(stderr))
        .env(BOOTSTRAP_FD_ENV, child_fd.to_string())
        .env(BOOTSTRAP_EXECUTION_ENV, execution.to_string());
    // SAFETY: runs in the post-fork child before exec. It only invokes
    // async-signal-safe libc calls and reports failures through Command.
    unsafe {
        command.pre_exec(move || {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            let flags = libc::fcntl(child_fd, libc::F_GETFD);
            if flags == -1 || libc::fcntl(child_fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) == -1
            {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = command.spawn().context("spawn resident supervisor")?;
    drop(child_socket);
    let result: BootstrapResult = blocking_io::read_frame(&mut parent, MAX_HANDSHAKE_FRAME_BYTES)
        .context("resident did not complete private bootstrap")?;
    match &result {
        BootstrapResult::Bound {
            execution: adopted, ..
        } if adopted == &execution => {}
        BootstrapResult::Bound { .. } => {
            bail!("resident adopted a different execution than the launcher minted")
        }
        BootstrapResult::Rejected { .. } => {}
    }
    Ok(LaunchedResident { child, result })
}

fn detached_command_args(
    args: impl IntoIterator<Item = std::ffi::OsString>,
    offline: bool,
) -> Vec<std::ffi::OsString> {
    let mut args = args.into_iter().collect::<Vec<_>>();
    if offline && !args.iter().any(|argument| argument == "--offline") {
        args.push("--offline".into());
    }
    args
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use super::detached_command_args;

    #[test]
    fn detached_offline_flag_is_explicit_and_unique() {
        let base = [OsString::from("run"), OsString::from("-d")];
        assert_eq!(
            detached_command_args(base.clone(), true),
            ["run", "-d", "--offline"]
                .into_iter()
                .map(OsString::from)
                .collect::<Vec<_>>()
        );
        let explicit = [OsString::from("run"), OsString::from("--offline")];
        assert_eq!(
            detached_command_args(explicit.clone(), true),
            explicit.into_iter().collect::<Vec<_>>()
        );
        assert_eq!(
            detached_command_args(base.clone(), false),
            base.into_iter().collect::<Vec<_>>()
        );
    }
}
