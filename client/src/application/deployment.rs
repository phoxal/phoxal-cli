//! Building and installing a runtime on a remote host over SSH.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use anyhow::{Context, Result, bail};
use phoxal_cli_project::shell_quote;

use crate::cli::context::AppContext;

pub(crate) const REMOTE_PHOXAL: &str = phoxal_cli_project::INSTALLED_CLIENT_BINARY;
pub(crate) const REMOTE_PHOXALD: &str = phoxal_cli_project::INSTALLED_DAEMON_BINARY;

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
        let host = self.target.clone();
        let output = output.to_path_buf();
        let offline = app.offline;
        let built = tokio::task::spawn_blocking(move || {
            phoxal_cli_project::build_bundle(phoxal_cli_project::BuildBundleRequest {
                target,
                backend: phoxal_cli_project::BuildBackend::Ssh { host, target: None },
                executor: crate::pair::PairExecutors::shared(),
                output: Some(output),
                publish: false,
                offline,
                reporter,
            })
        })
        .await?;
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

/// Require the remote host to carry the exact CLI pair.
///
/// The unit executes `phoxald`, so a host with only `phoxal` installed accepts
/// an install and then cannot execute it. Both halves are
/// checked here, before anything is built or copied.
pub(crate) fn require_remote_phoxal(target: &str) -> Result<()> {
    let output = remote_output(
        target,
        &format!(
            "test -x {REMOTE_PHOXAL} && sudo -n test -x {REMOTE_PHOXAL} && \
             test -x {REMOTE_PHOXALD} && sudo -n test -x {REMOTE_PHOXALD} && \
             {REMOTE_PHOXAL} --version && {REMOTE_PHOXALD} --version"
        ),
    )?;
    anyhow::ensure!(
        output.status.success(),
        "{target} does not have the phoxal CLI pair installed. `phoxal` and `phoxald` ship and \
         install together: place both verified Linux release binaries as `{REMOTE_PHOXAL}` and \
         `{REMOTE_PHOXALD}`, then run `sudo {REMOTE_PHOXAL} service install` and \
         `{REMOTE_PHOXAL} service status`; deploy never provisions the device"
    );
    verify_remote_pair_output(target, &output.stdout)
}

fn verify_remote_pair_output(target: &str, stdout: &[u8]) -> Result<()> {
    let stdout =
        String::from_utf8(stdout.to_vec()).context("remote version output was not UTF-8")?;
    let versions = stdout.lines().filter_map(|line| {
        let mut fields = line.split_whitespace();
        let binary = fields.next()?;
        matches!(binary, "phoxal" | "phoxald").then(|| (binary, fields.next()))
    });
    let mut client = None;
    let mut daemon = None;
    for (binary, version) in versions {
        match binary {
            "phoxal" => client = version,
            "phoxald" => daemon = version,
            _ => {}
        }
    }
    let expected = env!("CARGO_PKG_VERSION");
    anyhow::ensure!(
        client == Some(expected) && daemon == Some(expected),
        "{target} has a mixed or unsupported CLI pair: expected phoxal and phoxald {expected}, \
         found phoxal {} and phoxald {}",
        client.unwrap_or("<unreported>"),
        daemon.unwrap_or("<unreported>")
    );
    Ok(())
}

pub(crate) fn create_remote_temp(target: &str) -> Result<String> {
    let output = remote_output(target, "mktemp -d /tmp/phoxal-deploy.XXXXXX")?;
    anyhow::ensure!(
        output.status.success(),
        "failed to create remote temporary directory"
    );
    Ok(String::from_utf8(output.stdout)?.trim().to_string())
}

pub(crate) fn cleanup_remote_temp(target: &str, path: &str) -> Result<()> {
    // `rm -rf` is destructive and the path came back over a pipe, so the prefix
    // this call created is what it is allowed to remove.
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

    #[test]
    fn remote_pair_requires_both_exact_versions() {
        assert!(
            verify_remote_pair_output(
                "robot@host",
                format!(
                    "phoxal {} (linux-aarch64)\nphoxald {}\n",
                    env!("CARGO_PKG_VERSION"),
                    env!("CARGO_PKG_VERSION")
                )
                .as_bytes(),
            )
            .is_ok()
        );
        assert!(
            verify_remote_pair_output(
                "robot@host",
                format!("phoxal {}\nphoxald 0.0.0\n", env!("CARGO_PKG_VERSION")).as_bytes(),
            )
            .is_err()
        );
    }
}
