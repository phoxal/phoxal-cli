//! Deploy source snapshots or prebuilt archives through one remote installer.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use anyhow::{Context, Result, bail};
use clap::Args;

use crate::AppContext;

pub(crate) const REMOTE_TOOLCHAIN_PATH: &str = r#"export PATH="$HOME/.cargo/bin:$PATH""#;
pub(crate) const REMOTE_PHOXAL: &str = "/usr/local/bin/phoxal";

#[derive(Debug, Args)]
pub struct Deploy {
    #[arg(value_name = "USER@HOST")]
    target: String,
    #[arg(
        value_name = "PROJECT",
        help = "Source project to snapshot. Defaults to the discovered project."
    )]
    project: Option<PathBuf>,
    #[arg(
        long,
        value_name = "BUILD_PHOXAL",
        conflicts_with = "project",
        help = "Push a prebuilt archive; the remote host needs no Cargo or Git."
    )]
    build: Option<PathBuf>,
}

impl Deploy {
    pub async fn run(&self, app: &AppContext) -> Result<()> {
        validate_ssh_target(&self.target)?;
        require_remote_phoxal(&self.target)?;
        // Prove source-build capability before creating even a temporary
        // directory on the robot. The prebuilt path deliberately skips this.
        let source_target = if self.build.is_none() {
            Some(remote_toolchain_target(&self.target)?)
        } else {
            None
        };
        let remote_dir = create_remote_temp(&self.target)?;
        let result = if let Some(archive) = &self.build {
            self.deploy_prebuilt(archive, &remote_dir)
        } else {
            self.deploy_source(
                app,
                &remote_dir,
                source_target
                    .as_deref()
                    .expect("source target was preflighted"),
            )
        };
        let cleanup = cleanup_remote_temp(&self.target, &remote_dir);
        result?;
        cleanup?;
        app.ui.info(format!("deployed runtime to {}", self.target));
        Ok(())
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

    fn deploy_source(&self, app: &AppContext, remote_dir: &str, target_triple: &str) -> Result<()> {
        let project =
            crate::commands::resident::resolve_target(self.project.as_deref(), app.project.root())?
                .project;
        upload_source_payload(&self.target, &project, remote_dir)?;
        let source_dir = format!("{remote_dir}/source");
        let remote_archive = format!("{remote_dir}/build.phoxal");
        let command = format!(
            "{}; {}; {} build {} --target {} --output {}; {}",
            REMOTE_TOOLCHAIN_PATH,
            remote_unpack_source_command(remote_dir),
            REMOTE_PHOXAL,
            shell_quote(&source_dir),
            shell_quote(target_triple),
            shell_quote(&remote_archive),
            remote_install_command(&remote_archive),
        );
        run_remote(&self.target, &command).context("remote source build or install failed")
    }
}

/// Upload source and vendored artifacts as distinct payloads. The source
/// snapshot itself therefore remains free of `.phoxal` build state.
pub(crate) fn upload_source_payload(target: &str, project: &Path, remote_dir: &str) -> Result<()> {
    let snapshot = tempfile::Builder::new()
        .prefix("phoxal-deploy-source-")
        .tempdir()?;
    crate::commands::build::snapshot_source(project, snapshot.path())?;
    let artifact_payload = tempfile::Builder::new()
        .prefix("phoxal-deploy-artifacts-")
        .tempdir()?;
    let artifacts = project.join(".phoxal/artifacts");
    // Archive the vendored store itself rather than copying it through the
    // runtime-layout tree copier: each package's `active` entry is a relative
    // symlink to its immutable version, and that activation link is required
    // for remote resolution. `tar` preserves it verbatim. The empty temporary
    // directory remains the payload when the project has no vendored store.
    let artifact_root = if artifacts.is_dir() {
        artifacts.as_path()
    } else {
        artifact_payload.path()
    };
    let source_archive = archive_directory(snapshot.path(), "phoxal-source-")?;
    let artifacts_archive = archive_directory(artifact_root, "phoxal-artifacts-")?;
    for (local, remote_name) in [
        (source_archive.path(), "source.tar.gz"),
        (artifacts_archive.path(), "artifacts.tar.gz"),
    ] {
        run_local(
            "scp",
            &[
                "-q",
                local.to_string_lossy().as_ref(),
                &format!("{target}:{remote_dir}/{remote_name}"),
            ],
        )?;
    }
    Ok(())
}

fn archive_directory(root: &Path, prefix: &str) -> Result<tempfile::NamedTempFile> {
    let archive = tempfile::Builder::new()
        .prefix(prefix)
        .suffix(".tar.gz")
        .tempfile()?;
    run_local(
        "tar",
        &[
            "-czf",
            archive.path().to_string_lossy().as_ref(),
            "-C",
            root.to_string_lossy().as_ref(),
            ".",
        ],
    )?;
    Ok(archive)
}

pub(crate) fn remote_unpack_source_command(remote_dir: &str) -> String {
    let source_dir = format!("{remote_dir}/source");
    let artifacts_dir = format!("{source_dir}/.phoxal/artifacts");
    format!(
        "set -eu; mkdir {}; tar -xzf {} -C {}; mkdir -p {}; tar -xzf {} -C {}",
        shell_quote(&source_dir),
        shell_quote(&format!("{remote_dir}/source.tar.gz")),
        shell_quote(&source_dir),
        shell_quote(&artifacts_dir),
        shell_quote(&format!("{remote_dir}/artifacts.tar.gz")),
        shell_quote(&artifacts_dir),
    )
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

pub(crate) fn remote_toolchain_target(target: &str) -> Result<String> {
    let output = remote_output(
        target,
        &format!(
            "{REMOTE_TOOLCHAIN_PATH}; command -v cargo >/dev/null && command -v rustc >/dev/null && rustc -vV"
        ),
    )?;
    if !output.status.success() {
        let arch = remote_output(target, "uname -m")?;
        let arch = String::from_utf8_lossy(&arch.stdout).trim().to_string();
        let triple = match arch.as_str() {
            "aarch64" | "arm64" => "aarch64-unknown-linux-gnu",
            "x86_64" | "amd64" => "x86_64-unknown-linux-gnu",
            _ => "<robot-triple>",
        };
        bail!(
            "{target} is missing Cargo or rustc; run `phoxal build --target {triple}`, then `phoxal deploy {target} --build <archive>`"
        );
    }
    let stdout = String::from_utf8(output.stdout)?;
    stdout
        .lines()
        .find_map(|line| line.strip_prefix("host: "))
        .map(str::to_string)
        .context("remote `rustc -vV` did not report a host target triple")
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
    fn source_unpack_keeps_artifacts_outside_the_source_snapshot_payload() {
        let command = remote_unpack_source_command("/tmp/phoxal-deploy.ABC");
        assert!(command.contains("source.tar.gz"));
        assert!(command.contains("artifacts.tar.gz"));
        assert!(command.contains("source/.phoxal/artifacts"));
    }

    #[cfg(unix)]
    #[test]
    fn artifact_payload_preserves_the_active_version_symlink() -> Result<()> {
        let store = tempfile::tempdir()?;
        let package = store.path().join("phoxal/service-asset");
        std::fs::create_dir_all(package.join("versions/0.40.0"))?;
        std::fs::write(package.join("versions/0.40.0/phoxal-service-asset"), b"ELF")?;
        std::os::unix::fs::symlink("versions/0.40.0", package.join("active"))?;

        let archive = archive_directory(store.path(), "phoxal-artifacts-test-")?;
        let extracted = tempfile::tempdir()?;
        run_local(
            "tar",
            &[
                "-xzf",
                archive.path().to_string_lossy().as_ref(),
                "-C",
                extracted.path().to_string_lossy().as_ref(),
            ],
        )?;

        let active = extracted.path().join("phoxal/service-asset/active");
        assert!(std::fs::symlink_metadata(&active)?.file_type().is_symlink());
        assert_eq!(
            std::fs::read_link(&active)?,
            std::path::PathBuf::from("versions/0.40.0")
        );
        assert_eq!(std::fs::read(active.join("phoxal-service-asset"))?, b"ELF");
        Ok(())
    }
}
