//! Payload synchronization protocol and deploy transport implementation.

use super::{
    BootstrapScripts, DOWNLOAD_CONCURRENCY, DeployTransport, DownloadArtifact, HealthReport,
    HealthUnitReport, IDENTITY_DIR, OPT_ROOT, PAYLOAD_STAGING_PREFIX, RemoteProbe, RenderedPayload,
    SshTransport, SudoPassword, bootstrap_script, deploy_ssh_command, managed_unit_name,
    payload_opt, run_bounded, stage_official_fallback, sudo_bootstrap_args, sudo_validate_args,
};
use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use phoxal_cli_core::project::launch_plan::INFRASTRUCTURE_ROUTER;
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::ffi::OsString;
use std::io::Write;
use std::process::Stdio;
use std::time::Duration;

pub(crate) trait PayloadSyncRemote {
    fn remote_host(&self) -> &str;

    fn run_ssh_status<I, S>(&mut self, args: I) -> Result<()>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>;

    fn run_rsync<I, S>(&mut self, args: I) -> Result<()>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>;

    fn run_helper(&mut self, args: &[&str]) -> Result<()>;
}

impl PayloadSyncRemote for SshTransport {
    fn remote_host(&self) -> &str {
        &self.host
    }

    fn run_ssh_status<I, S>(&mut self, args: I) -> Result<()>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        SshTransport::ssh_status(self, args)
    }

    fn run_rsync<I, S>(&mut self, args: I) -> Result<()>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        SshTransport::rsync(self, args)
    }

    fn run_helper(&mut self, args: &[&str]) -> Result<()> {
        SshTransport::run_helper(self, args)
    }
}

pub(crate) fn remote_staging_dir(prefix: &str) -> String {
    format!("{prefix}{}", std::process::id())
}

pub(crate) fn sync_payload_via_helper<R>(
    remote: &mut R,
    payload: &RenderedPayload,
    remote_tmp: &str,
) -> Result<()>
where
    R: PayloadSyncRemote,
{
    remote.run_ssh_status(["rm", "-rf", remote_tmp])?;
    let install_result = (|| -> Result<()> {
        remote.run_ssh_status(["mkdir", "-p", remote_tmp])?;
        let remote_dest = OsString::from(format!("{}:{remote_tmp}/", remote.remote_host()));
        remote.run_rsync(vec![
            OsString::from("-az"),
            OsString::from("--delete"),
            payload_opt(payload.root.path()).join("").into_os_string(),
            remote_dest,
        ])?;
        remote.run_helper(&[
            "prepare-release",
            remote_tmp,
            &payload.install_plan.release_generation,
        ])?;
        Ok(())
    })();
    let _ = remote.run_ssh_status(["rm", "-rf", remote_tmp]);
    install_result?;
    sync_identity_files(remote, payload)
}

pub(crate) fn sync_identity_files<R>(remote: &mut R, payload: &RenderedPayload) -> Result<()>
where
    R: PayloadSyncRemote,
{
    if !payload.install_plan.identity_files.is_empty() {
        remote.run_ssh_status(["install", "-d", "-m", "0700", IDENTITY_DIR])?;
    }
    for identity in &payload.install_plan.identity_files {
        let remote_dest =
            OsString::from(format!("{}:{}", remote.remote_host(), identity.remote_path));
        remote.run_rsync(vec![
            OsString::from("-az"),
            identity.local_path.clone().into_os_string(),
            remote_dest,
        ])?;
        remote.run_ssh_status(["chmod", "0600", identity.remote_path.as_str()])?;
    }
    Ok(())
}

impl DeployTransport for SshTransport {
    fn probe(&mut self) -> Result<RemoteProbe> {
        let arch = self.ssh_stdout(["uname", "-m"])?.trim().to_string();
        let bootstrap_required = self
            .ssh_output(["test", "-d", OPT_ROOT])
            .map(|output| !output.status.success())
            .unwrap_or(true);
        let remote_user = self.ssh_stdout(["whoami"])?.trim().to_string();
        let sudo_noninteractive = self
            .ssh_output(["sudo", "-n", "true"])
            .map(|output| output.status.success())
            .unwrap_or(false);
        let helper_stale = self.probe_helper_stale();
        // Blanket passwordless sudo must NOT short-circuit this to true: a
        // device with blanket sudo but no `phoxal-deploy` membership (e.g.
        // bootstrapped under the old per-user model) needs the bootstrap
        // repair path to run so it converges to the group model - which it
        // can do non-interactively here since blanket sudo covers it.
        let helper_grant = self.probe_helper_grant();
        Ok(RemoteProbe {
            arch,
            bootstrap_required,
            remote_user,
            sudo_noninteractive,
            helper_grant,
            helper_stale,
        })
    }

    fn validate_sudo_password(&mut self, password: &SudoPassword) -> Result<bool> {
        self.ssh_output_with_password(sudo_validate_args(), password)
            .map(|output| output.status.success())
    }

    fn bootstrap(
        &mut self,
        helper: &BootstrapScripts,
        sudo_password: Option<&SudoPassword>,
    ) -> Result<()> {
        let script = bootstrap_script(helper);
        let remote_path = format!("/tmp/phoxal-bootstrap.{}.sh", std::process::id());

        // Transfer the script over a plain (non-sudo) ssh first. The sudo
        // password, when needed, is reserved for the script execution stdin.
        let mut upload_command = deploy_ssh_command();
        upload_command
            .arg(&self.host)
            .arg(format!("cat > {remote_path}"))
            .stdin(Stdio::piped());
        let mut upload = upload_command
            .spawn()
            .with_context(|| format!("failed to start bootstrap upload ssh {}", self.host))?;
        let mut upload_stdin = upload
            .stdin
            .take()
            .context("bootstrap upload child stdin was not available")?;
        upload_stdin
            .write_all(script.as_bytes())
            .context("failed to write bootstrap script")?;
        drop(upload_stdin);
        let upload_status = upload
            .wait()
            .context("failed to wait for bootstrap upload ssh")?;
        if !upload_status.success() {
            bail!(
                "failed to upload bootstrap script to {}: status {upload_status}",
                self.host
            );
        }

        let mut run = deploy_ssh_command();
        run.arg(&self.host).args(sudo_bootstrap_args(&remote_path));
        if sudo_password.is_some() {
            run.stdin(Stdio::piped());
        } else {
            run.stdin(Stdio::null());
        }
        let mut child = self
            .ui
            .command_spawn(&mut run)
            .with_context(|| format!("failed to run bootstrap script on {}", self.host))?;
        if let Some(password) = sudo_password {
            let mut stdin = child
                .stdin
                .take()
                .context("bootstrap child stdin was not available")?;
            password.write_with_newline(&mut stdin)?;
            drop(stdin);
        }
        let run_status = child
            .wait()
            .with_context(|| format!("failed to wait for bootstrap script on {}", self.host))?;

        // Best-effort cleanup: report bootstrap's own failure first, since
        // that's the actionable error; a stray temp file is not.
        let _ = self.ssh_status(["rm", "-f", &remote_path]);

        if run_status.success() {
            Ok(())
        } else {
            bail!("remote bootstrap failed with status {run_status}")
        }
    }

    fn list_installed_units(&mut self) -> Result<Vec<String>> {
        let output = self.ssh_output([
            "systemctl",
            "list-unit-files",
            "phoxal*",
            "--no-legend",
            "--no-pager",
        ])?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        // systemctl list-unit-files exits 1 with empty output when the pattern
        // matches nothing - the normal state of a freshly bootstrapped host.
        if !output.status.success() && (!stdout.trim().is_empty() || !stderr.trim().is_empty()) {
            bail!(
                "failed to list installed phoxal units: ssh {} failed with status {}\nstdout:\n{stdout}\nstderr:\n{stderr}",
                self.host,
                output.status
            );
        }
        Ok(stdout
            .lines()
            .filter_map(|line| line.split_whitespace().next().map(str::to_string))
            .filter(|unit| managed_unit_name(unit))
            .collect())
    }

    fn github_release_reachable(&mut self, url: &str) -> Result<bool> {
        self.github_release_reachable_from_robot(url)
    }

    fn prepare_host_transfer_fallback(
        &mut self,
        payload: &mut RenderedPayload,
        ui: &crate::Ui,
    ) -> Result<()> {
        stage_official_fallback(payload, ui)
    }

    fn sync_payload(&mut self, payload: &RenderedPayload) -> Result<()> {
        let remote_tmp = remote_staging_dir(PAYLOAD_STAGING_PREFIX);
        sync_payload_via_helper(self, payload, &remote_tmp)
    }

    fn download_official_artifacts(
        &mut self,
        generation: &str,
        artifacts: &[DownloadArtifact],
    ) -> Result<()> {
        let transport = self.clone();
        run_bounded(artifacts, DOWNLOAD_CONCURRENCY, |artifact| {
            transport.download_artifact(generation, artifact)
        })
    }

    fn install_units(&mut self, payload: &RenderedPayload, stale_units: &[String]) -> Result<()> {
        let _ = stale_units;
        for unit in &payload.unit_names {
            self.run_helper(&["install-unit", unit])?;
        }
        self.pending_units = payload.unit_names.clone();
        Ok(())
    }

    fn activate_release(&mut self, generation: &str) -> Result<()> {
        self.run_helper(&["activate-release", generation])
    }

    fn rollback_release(&mut self) -> Result<()> {
        self.run_helper(&["rollback-release"])
    }

    fn finalize_units(&mut self, stale_units: &[String]) -> Result<()> {
        for unit in stale_units {
            self.run_helper(&["disable-unit", unit])?;
            self.run_helper(&["remove-unit", unit])?;
        }
        self.run_helper(&["daemon-reload"])
    }

    fn restart(&mut self) -> Result<()> {
        self.run_helper(&["daemon-reload"])?;
        for unit in self.pending_units.clone() {
            self.run_helper(&["enable-unit", &unit])?;
        }
        self.run_helper(&["restart-target"])
    }

    fn health_report(&mut self, units: &[String], deadline: Duration) -> Result<HealthReport> {
        let start = std::time::Instant::now();
        loop {
            let report = self.collect_health(units)?;
            if report.is_ok() || start.elapsed() >= deadline {
                return Ok(report);
            }
            std::thread::sleep(Duration::from_secs(1));
        }
    }
}

impl SshTransport {
    fn collect_health(&self, units: &[String]) -> Result<HealthReport> {
        let mut reports = Vec::new();
        for unit in units {
            let active = self
                .ssh_output(["systemctl", "is-active", unit])
                .map(|output| output.status.success())
                .unwrap_or(false);
            let show = self
                .ssh_stdout([
                    "systemctl",
                    "show",
                    unit,
                    "-p",
                    "ActiveState",
                    "-p",
                    "SubState",
                ])
                .unwrap_or_default();
            let fields = show
                .lines()
                .filter_map(|line| line.split_once('='))
                .map(|(key, value)| (key.to_string(), value.to_string()))
                .collect::<BTreeMap<_, _>>();
            let journal_excerpt = if active {
                Vec::new()
            } else {
                self.ssh_stdout([
                    "journalctl",
                    "-u",
                    unit,
                    "-n",
                    "20",
                    "--no-pager",
                    "--output",
                    "cat",
                ])
                .unwrap_or_default()
                .lines()
                .map(str::to_string)
                .collect()
            };
            reports.push(HealthUnitReport {
                unit: unit.clone(),
                participant: participant_from_unit(unit),
                ready: active,
                active_state: fields
                    .get("ActiveState")
                    .cloned()
                    .unwrap_or_else(|| if active { "active" } else { "unknown" }.to_string()),
                sub_state: fields.get("SubState").cloned().unwrap_or_default(),
                journal_excerpt,
            });
        }
        Ok(HealthReport { units: reports })
    }
}

pub(crate) fn participant_from_unit(unit: &str) -> Option<String> {
    unit.strip_prefix("phoxal-participant-")
        .and_then(|rest| rest.strip_suffix(".service"))
        .map(str::to_string)
        .or_else(|| (unit == "phoxal-router.service").then(|| INFRASTRUCTURE_ROUTER.to_string()))
}
