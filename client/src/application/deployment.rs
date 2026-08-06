//! Building and installing a runtime on a remote host over SSH.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use anyhow::{Context, Result, bail};

use crate::cli::AppContext;

pub(crate) const REMOTE_PHOXAL: &str = "/usr/local/bin/phoxal";

pub(crate) struct DeployRequest {
    pub(crate) target: String,
    pub(crate) project: Option<PathBuf>,
    pub(crate) build: Option<PathBuf>,
}

impl DeployRequest {
    pub async fn run(&self, app: &AppContext) -> Result<()> {
        validate_ssh_target(&self.target)?;
        require_remote_phoxal(&self.target)?;
        if let Some(archive) = &self.build {
            self.deploy_archive(archive)?;
        } else {
            let output = tempfile::Builder::new()
                .prefix("phoxal-deploy-build-")
                .suffix(".build.phoxal")
                .tempfile()?;
            let built = self.build_source(app, output.path()).await?;

            // The project build use case deliberately returns one locally
            // verified archive regardless of backend. Deploy sends that exact
            // artifact through the same remote installer as `--build`, trading
            // one remote-local-remote transfer for a single validation and
            // installation contract.
            self.deploy_archive(&built.archive)?;
        }
        app.ui.info(format!("deployed runtime to {}", self.target));
        Ok(())
    }

    fn deploy_archive(&self, archive: &Path) -> Result<()> {
        // Create the remote deploy directory only after all source-build
        // capability checks and compilation have succeeded.
        let remote_dir = create_remote_temp(&self.target)?;
        let result = self.deploy_prebuilt(archive, &remote_dir);
        let cleanup = cleanup_remote_temp(&self.target, &remote_dir);
        result?;
        cleanup
    }

    fn deploy_prebuilt(&self, archive: &Path, remote_dir: &str) -> Result<()> {
        let archive = archive
            .canonicalize()
            .with_context(|| format!("failed to resolve {}", archive.display()))?;
        anyhow::ensure!(archive.is_file(), "{} is not a file", archive.display());
        let remote_archive = format!("{remote_dir}/build.phoxal");
        run_local(
            "scp",
            &[
                "-q",
                archive.to_string_lossy().as_ref(),
                &format!("{}:{remote_archive}", self.target),
            ],
        )?;
        run_remote(&self.target, &remote_install_command(&remote_archive))
            .context("remote installer rejected the prebuilt runtime")
    }

    async fn build_source(
        &self,
        app: &AppContext,
        output: &Path,
    ) -> Result<phoxal_cli_project::BuiltBundle> {
        let target =
            phoxal_cli_project::resolve_target(self.project.as_deref(), app.project.root())?;
        let _lock = crate::lock::ProjectLock::acquire(crate::lock::ProjectLockIdentity::resolve(
            &target.logical_root,
            crate::lock::ProjectOperation::Build,
        ))
        .context("failed to acquire the project lock for deploy")?;
        let (reporter, signal_task) =
            crate::cli::output::progress::cancellable_preparation_reporter(app.ui);
        let built = phoxal_cli_project::build_bundle(phoxal_cli_project::BuildBundleRequest {
            target,
            backend: phoxal_cli_project::BuildBackend::Ssh {
                host: self.target.clone(),
                target: None,
            },
            output: Some(output.to_path_buf()),
            publish: false,
            offline: app.offline,
            reporter,
        })
        .await;
        signal_task.abort();
        built.context("remote source build failed")
    }
}

pub(crate) fn remote_install_command(archive: &str) -> String {
    format!("sudo -n {REMOTE_PHOXAL} install {}", shell_quote(archive))
}

pub(crate) fn validate_ssh_target(target: &str) -> Result<()> {
    let Some((user, host)) = target.split_once('@') else {
        bail!("deploy target must be `user@host`");
    };
    anyhow::ensure!(
        !user.is_empty()
            && !host.is_empty()
            && !host.contains('@')
            && target
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"._@:-".contains(&byte)),
        "invalid deploy target `{target}`; expected `user@host`"
    );
    Ok(())
}

pub(crate) fn require_remote_phoxal(target: &str) -> Result<()> {
    let output = remote_output(
        target,
        &format!("test -x {REMOTE_PHOXAL} && sudo -n test -x {REMOTE_PHOXAL}"),
    )?;
    anyhow::ensure!(
        output.status.success(),
        "{target} does not have phoxal installed. Install the verified Linux release binary as \
         `/usr/local/bin/phoxal`, then run `sudo /usr/local/bin/phoxal service install` and \
         `/usr/local/bin/phoxal service status`; deploy never provisions the device"
    );
    Ok(())
}

pub(crate) fn create_remote_temp(target: &str) -> Result<String> {
    let output = remote_output(target, "mktemp -d /tmp/phoxal-deploy.XXXXXX")?;
    anyhow::ensure!(
        output.status.success(),
        "failed to create remote temporary directory"
    );
    let path = String::from_utf8(output.stdout)?.trim().to_string();
    anyhow::ensure!(
        path.starts_with("/tmp/phoxal-deploy.")
            && path
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"/._-".contains(&byte)),
        "remote host returned unsafe temporary path `{path}`"
    );
    Ok(path)
}

pub(crate) fn cleanup_remote_temp(target: &str, path: &str) -> Result<()> {
    anyhow::ensure!(
        path.starts_with("/tmp/phoxal-deploy."),
        "refusing to clean unexpected remote path `{path}`"
    );
    run_remote(target, &format!("rm -rf -- {}", shell_quote(path)))
}

pub(crate) fn run_remote(target: &str, command: &str) -> Result<()> {
    let output = remote_output(target, command)?;
    if output.status.success() {
        return Ok(());
    }
    bail!(
        "ssh command failed with {}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    )
}

fn remote_output(target: &str, command: &str) -> Result<Output> {
    Command::new("ssh")
        .args(["-o", "BatchMode=yes", target, command])
        .output()
        .with_context(|| format!("failed to run ssh for {target}"))
}

pub(crate) fn run_local(program: &str, args: &[&str]) -> Result<()> {
    let status = Command::new(program).args(args).status()?;
    anyhow::ensure!(
        status.success(),
        "{} {} failed with {status}",
        program,
        args.join(" ")
    );
    Ok(())
}

pub(crate) fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

pub(crate) async fn deploy_command(
    app: &AppContext,
    target: String,
    project: Option<PathBuf>,
    build: Option<PathBuf>,
) -> Result<()> {
    DeployRequest {
        target,
        project,
        build,
    }
    .run(app)
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deploy_target_is_exactly_user_at_host() {
        assert!(validate_ssh_target("robot@jetson-nano-orin").is_ok());
        assert!(validate_ssh_target("jetson-nano-orin").is_err());
        assert!(validate_ssh_target("robot@host;reboot").is_err());
    }

    #[test]
    fn remote_cleanup_is_prefix_fenced_without_running_ssh() {
        assert!(cleanup_remote_temp("robot@host", "/").is_err());
    }

    #[test]
    fn source_and_prebuilt_modes_share_the_exact_installer_command() {
        assert_eq!(
            remote_install_command("/tmp/phoxal-deploy.ABC/build.phoxal"),
            "sudo -n /usr/local/bin/phoxal install '/tmp/phoxal-deploy.ABC/build.phoxal'"
        );
    }
}
