//! Transport-independent deployment execution and health verification.

use super::{
    DeployOptions, DeployReport, DeployTransport, HealthReport, LocalSudoPasswordSource,
    OfficialDelivery, RemoteProbe, SUDO_PASSWORD_ENV, SudoPassword, SudoPasswordSource,
    format_health_failure, prepare_deploy, report_from_payload, stale_units,
    validate_deploy_options, validate_download_descriptor,
};
use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use anyhow::bail;
use phoxal_cli_core::deploy::target_from_uname_arch;
use std::ffi::OsStr;
use std::fs;
use std::fs::OpenOptions;
use std::io::Read;
use std::io::Write;
#[cfg(unix)]
use std::os::fd::AsRawFd;
#[cfg(unix)]
use std::os::fd::RawFd;
#[cfg(unix)]
use std::os::unix::ffi::OsStringExt;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

pub(crate) fn deploy_with_transport<T: DeployTransport>(
    project_start: &Path,
    options: &DeployOptions,
    transport: &mut T,
    local_tty_available: bool,
    ui: &crate::Ui,
) -> Result<DeployReport> {
    let mut sudo_passwords = LocalSudoPasswordSource;
    deploy_with_transport_with_sudo(
        project_start,
        options,
        transport,
        local_tty_available,
        &mut sudo_passwords,
        ui,
    )
}

pub(crate) fn deploy_with_transport_with_sudo<T, S>(
    project_start: &Path,
    options: &DeployOptions,
    transport: &mut T,
    local_tty_available: bool,
    sudo_passwords: &mut S,
    ui: &crate::Ui,
) -> Result<DeployReport>
where
    T: DeployTransport,
    S: SudoPasswordSource + ?Sized,
{
    validate_deploy_options(options)?;
    let probe = transport.probe().context("failed to probe deploy host")?;
    let target = target_from_uname_arch(&probe.arch)?;
    let host = options
        .host
        .as_deref()
        .context("deploy requires <user@host> unless --dry-run is set")?;
    let sudo_password =
        ensure_sudo_will_succeed(host, &probe, local_tty_available, sudo_passwords, transport)?;
    let mut payload = prepare_deploy(project_start, options, target, true, &probe.remote_user, ui)?;

    if probe.root_work_required() {
        transport
            .bootstrap(&payload.bootstrap, sudo_password.as_ref())
            .context("failed to bootstrap remote phoxal install")?;
    }
    drop(sudo_password);
    let installed = transport
        .list_installed_units()
        .context("failed to list installed phoxal units")?;
    let stale = stale_units(&installed, &payload.unit_names);
    payload.install_plan.stale_units_to_remove = stale.clone();
    validate_download_descriptor(&payload.download_descriptor)?;

    let reachability_url = payload
        .download_descriptor
        .artifacts
        .first()
        .map(|artifact| artifact.url.as_str())
        .filter(|url| !url.is_empty())
        .context("resolved deploy has no published official release URL")?;
    let delivery = if transport
        .github_release_reachable(reachability_url)
        .context("failed to preflight GitHub release reachability from the robot")?
    {
        OfficialDelivery::RobotDownload
    } else {
        transport
            .prepare_host_transfer_fallback(&mut payload, ui)
            .context("failed to prepare host-transfer fallback artifacts")?;
        OfficialDelivery::HostTransferFallback
    };
    payload.delivery = Some(delivery);

    transport
        .sync_payload(&payload)
        .context("failed to sync phoxal payload")?;
    if delivery == OfficialDelivery::RobotDownload {
        transport
            .download_official_artifacts(
                &payload.install_plan.release_generation,
                &payload.download_descriptor.artifacts,
            )
            .context("robot failed to download official release artifacts")?;
    }
    transport
        .install_units(&payload, &stale)
        .context("failed to install systemd units")?;
    transport
        .activate_release(&payload.install_plan.release_generation)
        .context("failed to activate transactional release")?;
    let health = match restart_and_check(transport, &payload.unit_names, options.health_timeout) {
        Ok(health) => health,
        Err(deploy_error) => {
            let rollback = transport
                .rollback_release()
                .and_then(|()| transport.restart());
            return match rollback {
                Ok(()) => Err(anyhow!(
                    "{deploy_error:#}\nrelease rolled back to the previous generation"
                )),
                Err(rollback_error) => Err(anyhow!(
                    "{deploy_error:#}\nrelease activation failed and rollback also failed: {rollback_error:#}"
                )),
            };
        }
    };
    transport
        .finalize_units(&stale)
        .context("failed to finalize systemd units after healthy activation")?;
    Ok(report_from_payload("deploy", payload, Some(health)))
}

pub(crate) fn restart_and_check<T: DeployTransport>(
    transport: &mut T,
    units: &[String],
    deadline: Duration,
) -> Result<HealthReport> {
    transport
        .restart()
        .context("failed to restart phoxal.target")?;
    let health = transport
        .health_report(units, deadline)
        .context("failed to collect deploy health")?;
    if !health.is_ok() {
        bail!("{}", format_health_failure(&health));
    }
    Ok(health)
}

/// Fail before cross-building/packaging/rsyncing anything if sudo on the
/// target will never succeed. Decision table:
///
/// 1. `sudo -n true` works (blanket NOPASSWD or a cached credential):
///    proceed - bootstrap or grant repair can run non-interactively.
/// 2. No blanket sudo, but the group-model grant is fully in place for this
///    user (`helper_grant`: helper installed, user enrolled in
///    `phoxal-deploy`, sudoers fragment authorizes the call), and the helper
///    hash matches this build: proceed - the steady-state deploy needs no
///    root work; every `run_helper` call goes through the fragment.
/// 3. Root work is required (first bootstrap, a stale/missing grant, or a
///    stale helper that the bootstrap script repairs) and `PHOXAL_SUDO_PASSWORD`
///    is set or local `/dev/tty` is available: validate a password now, then
///    proceed and feed it to bootstrap over child stdin.
/// 4. Root work is required and there is no password env var and no local
///    `/dev/tty`: fail now, before doing any work, with all remedies.
pub(crate) fn ensure_sudo_will_succeed<T, S>(
    host: &str,
    probe: &RemoteProbe,
    local_tty_available: bool,
    sudo_passwords: &mut S,
    transport: &mut T,
) -> Result<Option<SudoPassword>>
where
    T: DeployTransport,
    S: SudoPasswordSource + ?Sized,
{
    if probe.sudo_noninteractive {
        return Ok(None);
    }
    if !probe.root_work_required() {
        return Ok(None);
    }
    if let Some(password) = sudo_passwords.password_from_env() {
        if transport
            .validate_sudo_password(&password)
            .with_context(|| format!("failed to validate sudo password on {host}"))?
        {
            return Ok(Some(password));
        }
        bail!(
            "DeploySudoPasswordRejected: {SUDO_PASSWORD_ENV} did not validate for {user} on {host}.",
            user = probe.remote_user,
        );
    }
    if local_tty_available {
        let prompt = sudo_password_prompt(&probe.remote_user, host);
        for _ in 0..2 {
            let password = sudo_passwords.read_password(&prompt).with_context(|| {
                format!(
                    "failed to read sudo password for {user} on {host}",
                    user = probe.remote_user
                )
            })?;
            if transport
                .validate_sudo_password(&password)
                .with_context(|| format!("failed to validate sudo password on {host}"))?
            {
                return Ok(Some(password));
            }
        }
        bail!(
            "DeploySudoPasswordRejected: sudo password validation failed for {user} on {host} after 2 attempts.",
            user = probe.remote_user,
        );
    }
    let root_work = if probe.bootstrap_required {
        "needs root once (first deploy: install /opt/phoxal, the phoxal-systemd-helper, and the phoxal-deploy group sudoers grant)"
    } else if probe.helper_stale {
        "needs root once (repair: the installed phoxal-systemd-helper is stale for this phoxal-cli build, so the deploy must rewrite the helper and the phoxal-deploy group sudoers grant)"
    } else {
        "needs root once (repair: this user is not covered by the phoxal-deploy group grant, so the deploy must install the phoxal-deploy group sudoers grant and add this user to the group)"
    };
    bail!(
        "DeploySudoRequiresPassword: {host} {root_work} and sudo is not passwordless for {user}. Fix: rerun `phoxal-cli deploy` interactively (from a real TTY so phoxal-cli can read /dev/tty), pre-authorize {user} on {host} with a NOPASSWD sudoers entry, or for automation set {SUDO_PASSWORD_ENV} for this command (NOPASSWD or an interactive run is preferred).",
        user = probe.remote_user,
    )
}

pub(crate) fn sudo_password_prompt(user: &str, host: &str) -> String {
    format!("[sudo] password for {user} on {host}:")
}

#[cfg(unix)]
pub(crate) fn sudo_password_from_env() -> Option<SudoPassword> {
    std::env::var_os(SUDO_PASSWORD_ENV).map(|password| SudoPassword::new(password.into_vec()))
}

#[cfg(not(unix))]
pub(crate) fn sudo_password_from_env() -> Option<SudoPassword> {
    std::env::var_os(SUDO_PASSWORD_ENV)
        .map(|password| SudoPassword::new(password.to_string_lossy().into_owned().into_bytes()))
}

pub(crate) fn local_tty_available() -> bool {
    open_tty().is_ok()
}

#[cfg(unix)]
pub(crate) fn open_tty() -> std::io::Result<fs::File> {
    OpenOptions::new().read(true).write(true).open("/dev/tty")
}

#[cfg(not(unix))]
pub(crate) fn open_tty() -> std::io::Result<fs::File> {
    Err(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        "/dev/tty is not available on this platform",
    ))
}

pub(crate) fn read_password_from_tty(prompt: &str) -> Result<SudoPassword> {
    let mut tty = open_tty().context("failed to open /dev/tty for sudo password prompt")?;
    tty.write_all(prompt.as_bytes())
        .context("failed to write sudo password prompt to /dev/tty")?;
    tty.flush()
        .context("failed to flush sudo password prompt to /dev/tty")?;
    let mut password = SudoPassword::new(Vec::new());
    {
        let _echo_guard = TtyEchoGuard::disable(&tty).context("failed to disable /dev/tty echo")?;
        loop {
            let mut byte = [0_u8; 1];
            let read = tty
                .read(&mut byte)
                .context("failed to read sudo password from /dev/tty")?;
            if read == 0 {
                bail!("failed to read sudo password from /dev/tty: EOF");
            }
            match byte[0] {
                b'\n' | b'\r' => break,
                value => password.push(value),
            }
        }
    }
    tty.write_all(b"\n")
        .context("failed to finish sudo password prompt on /dev/tty")?;
    Ok(password)
}

#[cfg(unix)]
pub(crate) struct TtyEchoGuard {
    fd: RawFd,
    original: libc::termios,
}

#[cfg(unix)]
impl TtyEchoGuard {
    fn disable(tty: &fs::File) -> Result<Self> {
        let fd = tty.as_raw_fd();
        let mut original = std::mem::MaybeUninit::<libc::termios>::uninit();
        if unsafe { libc::tcgetattr(fd, original.as_mut_ptr()) } != 0 {
            return Err(std::io::Error::last_os_error()).context("tcgetattr failed");
        }
        let original = unsafe { original.assume_init() };
        let mut no_echo = original;
        no_echo.c_lflag &= !(libc::ECHO as libc::tcflag_t);
        if unsafe { libc::tcsetattr(fd, libc::TCSAFLUSH, &no_echo) } != 0 {
            return Err(std::io::Error::last_os_error()).context("tcsetattr failed");
        }
        Ok(Self { fd, original })
    }
}

#[cfg(unix)]
impl Drop for TtyEchoGuard {
    fn drop(&mut self) {
        let _ = unsafe { libc::tcsetattr(self.fd, libc::TCSAFLUSH, &self.original) };
    }
}

#[cfg(not(unix))]
pub(crate) struct TtyEchoGuard;

#[cfg(not(unix))]
impl TtyEchoGuard {
    fn disable(_tty: &fs::File) -> Result<Self> {
        bail!("/dev/tty password prompting is only supported on Unix platforms")
    }
}

pub(crate) fn deploy_command(program: impl AsRef<OsStr>) -> Command {
    let mut command = Command::new(program);
    command.env_remove(SUDO_PASSWORD_ENV);
    command
}

/// Each remote command must create a fresh SSH login session. In particular,
/// the first helper call after bootstrap must not reuse a ControlMaster whose
/// server-side process still has the supplementary groups from before
/// `usermod -aG phoxal-deploy` ran.
pub(crate) fn deploy_ssh_command() -> Command {
    let mut command = deploy_command("ssh");
    command.args([
        "-o",
        "ControlMaster=no",
        "-o",
        "ControlPath=none",
        "-o",
        "ControlPersist=no",
    ]);
    command
}

// The prompt must be a single non-empty token: these argv vectors travel over
// `ssh <host> <args...>`, which flattens them into one shell line - an empty
// `-p ""` argument vanishes and `-p` then swallows the next token as the
// prompt (turning the script path into the command). The prompt itself only
// goes to the remote stderr; the password always arrives via stdin (-S).
pub(crate) const SUDO_STDIN_PROMPT: &str = "phoxal-sudo-password:";

pub(crate) fn sudo_validate_args() -> [&'static str; 5] {
    ["sudo", "-S", "-p", SUDO_STDIN_PROMPT, "-v"]
}

pub(crate) fn sudo_bootstrap_args(remote_path: &str) -> Vec<&str> {
    vec!["sudo", "-S", "-p", SUDO_STDIN_PROMPT, "sh", remote_path]
}
