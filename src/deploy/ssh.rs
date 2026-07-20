//! SSH transport command execution and remote probing.

use super::{
    DownloadArtifact, HELPER_PATH, SudoPassword, deploy_command, deploy_ssh_command,
    helper_script_sha256,
};
use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use std::ffi::OsStr;
use std::io::Write;
use std::process::Stdio;

#[derive(Debug, Clone)]
pub(crate) struct SshTransport {
    pub(crate) host: String,
    pub(crate) ui: crate::Ui,
    pub(crate) pending_units: Vec<String>,
}

impl SshTransport {
    pub(crate) fn new(host: String, ui: crate::Ui) -> Self {
        Self {
            host,
            ui,
            pending_units: Vec::new(),
        }
    }

    pub(crate) fn ssh_output<I, S>(&self, args: I) -> Result<std::process::Output>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut command = deploy_ssh_command();
        command.arg(&self.host).args(args);
        command
            .output()
            .with_context(|| format!("failed to run ssh {}", self.host))
    }

    pub(crate) fn ssh_output_with_password<I, S>(
        &self,
        args: I,
        password: &SudoPassword,
    ) -> Result<std::process::Output>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut command = deploy_ssh_command();
        command
            .arg(&self.host)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command
            .spawn()
            .with_context(|| format!("failed to run ssh {}", self.host))?;
        let mut stdin = child
            .stdin
            .take()
            .context("sudo validation child stdin was not available")?;
        password.write_with_newline(&mut stdin)?;
        drop(stdin);
        child
            .wait_with_output()
            .with_context(|| format!("failed to wait for ssh {}", self.host))
    }

    pub(crate) fn ssh_status<I, S>(&self, args: I) -> Result<()>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let output = self.ssh_output(args)?;
        if output.status.success() {
            return Ok(());
        }
        bail!(
            "ssh {} failed with status {}\nstdout:\n{}\nstderr:\n{}",
            self.host,
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    }

    pub(crate) fn ssh_stdout<I, S>(&self, args: I) -> Result<String>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let output = self.ssh_output(args)?;
        if output.status.success() {
            return String::from_utf8(output.stdout)
                .with_context(|| format!("ssh {} wrote non-UTF8 stdout", self.host));
        }
        bail!(
            "ssh {} failed with status {}\nstdout:\n{}\nstderr:\n{}",
            self.host,
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    }

    pub(crate) fn rsync<I, S>(&self, args: I) -> Result<()>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut command = deploy_command("rsync");
        command.args(args);
        let status = self.ui.command_status(&mut command)?;
        if status.success() {
            Ok(())
        } else {
            bail!("rsync failed with status {status}")
        }
    }

    pub(crate) fn run_helper(&self, args: &[&str]) -> Result<()> {
        let mut command = vec!["sudo", HELPER_PATH];
        command.extend_from_slice(args);
        self.ssh_status(command)
    }

    pub(crate) fn github_release_reachable_from_robot(&self, url: &str) -> Result<bool> {
        let mut command = deploy_ssh_command();
        command
            .arg(&self.host)
            .arg("url=$(cat); curl --head --fail --location --silent --show-error --connect-timeout 5 --max-time 15 \"$url\" >/dev/null")
            .stdin(Stdio::piped());
        let mut child = command.spawn().with_context(|| {
            format!("failed to start GitHub reachability probe on {}", self.host)
        })?;
        child
            .stdin
            .take()
            .context("reachability probe stdin was unavailable")?
            .write_all(url.as_bytes())?;
        Ok(child.wait()?.success())
    }

    pub(crate) fn download_artifact(
        &self,
        generation: &str,
        artifact: &DownloadArtifact,
    ) -> Result<()> {
        let size = artifact.size.to_string();
        let mut command = deploy_ssh_command();
        command
            .arg(&self.host)
            .args([
                "sudo",
                HELPER_PATH,
                "download-artifact",
                generation,
                size.as_str(),
                artifact.sha256.as_str(),
                artifact.archive_binary_name.as_str(),
                artifact.install_binary_name.as_str(),
            ])
            .stdin(Stdio::piped());
        let mut child = command
            .spawn()
            .with_context(|| format!("failed to start robot download for {}", artifact.package))?;
        child
            .stdin
            .take()
            .context("robot download stdin was unavailable")?
            .write_all(artifact.url.as_bytes())?;
        let status = child.wait()?;
        if status.success() {
            Ok(())
        } else {
            bail!(
                "robot download failed for {} {} with status {status}",
                artifact.package,
                artifact.version
            )
        }
    }

    /// `sudo -n true` tests blanket sudo, but the group-model grant needs
    /// all three of: the helper installed and executable, this user enrolled
    /// in the `phoxal-deploy` group, and the sudoers fragment actually
    /// authorizing the call for that group - so probe the grant itself
    /// rather than infer it from blanket sudo. Running the installed helper
    /// with no arguments hits its unknown-verb branch and exits 64, so exit
    /// 0 or 64 proves sudo authorized this user for the helper; a sudo
    /// password failure exits 1 without ever running the helper.
    pub(crate) fn probe_helper_grant(&self) -> bool {
        let helper_installed = self
            .ssh_output(["test", "-x", HELPER_PATH])
            .map(|output| output.status.success())
            .unwrap_or(false);
        if !helper_installed {
            return false;
        }
        if !self.probe_phoxal_deploy_group_membership() {
            return false;
        }
        self.ssh_output(["sudo", "-n", HELPER_PATH])
            .map(|output| matches!(output.status.code(), Some(0) | Some(64)))
            .unwrap_or(false)
    }

    /// Exact-word match against `id -nG` output - a substring match would
    /// wrongly accept e.g. a group named `phoxal-deploy-readonly`.
    pub(crate) fn probe_phoxal_deploy_group_membership(&self) -> bool {
        self.ssh_stdout(["id", "-nG"])
            .map(|output| {
                output
                    .split_whitespace()
                    .any(|group| group == "phoxal-deploy")
            })
            .unwrap_or(false)
    }

    pub(crate) fn probe_helper_stale(&self) -> bool {
        let expected = helper_script_sha256();
        match self.ssh_stdout(["sha256sum", HELPER_PATH]) {
            Ok(output) => output.split_whitespace().next() != Some(expected.as_str()),
            Err(_) => true,
        }
    }
}
