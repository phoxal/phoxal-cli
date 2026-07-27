//! Deploy source snapshots or prebuilt archives through one remote installer.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use anyhow::{Context, Result, bail};
use clap::Args;

use crate::AppContext;

pub(crate) const REMOTE_TOOLCHAIN_PATH: &str = r#"export PATH="$HOME/.cargo/bin:$PATH""#;
pub(crate) const REMOTE_PHOXAL: &str = "/usr/local/bin/phoxal";
const ARCHIVE_TAR_OPTIONS: [&str; 2] = ["--no-xattrs", "-czf"];
const ARCHIVE_TAR_ENV: (&str, &str) = ("COPYFILE_DISABLE", "1");

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

/// Upload the source snapshot. Official runtimes no longer vendor into the
/// project (organization#951 WS4): the remote `phoxal build` this snapshot
/// feeds materializes them itself, via `cargo install` against the registry,
/// exactly like a local build - `deploy_source` already requires a native
/// Cargo/rustc toolchain on the remote host (see `remote_toolchain_target`),
/// so that install is native there too, never a host cross-compile.
pub(crate) fn upload_source_payload(target: &str, project: &Path, remote_dir: &str) -> Result<()> {
    let snapshot = tempfile::Builder::new()
        .prefix("phoxal-deploy-source-")
        .tempdir()?;
    crate::commands::build::snapshot_source(project, snapshot.path())?;
    let source_archive = archive_directory(snapshot.path(), "phoxal-source-")?;
    run_local(
        "scp",
        &[
            "-q",
            source_archive.path().to_string_lossy().as_ref(),
            &format!("{target}:{remote_dir}/source.tar.gz"),
        ],
    )
}

fn archive_directory(root: &Path, prefix: &str) -> Result<tempfile::NamedTempFile> {
    let archive = tempfile::Builder::new()
        .prefix(prefix)
        .suffix(".tar.gz")
        .tempfile()?;
    let status = Command::new("tar")
        .env(ARCHIVE_TAR_ENV.0, ARCHIVE_TAR_ENV.1)
        .args(ARCHIVE_TAR_OPTIONS)
        .arg(archive.path())
        .arg("-C")
        .arg(root)
        .arg(".")
        .status()?;
    anyhow::ensure!(
        status.success(),
        "tar archive creation failed with {status}"
    );
    Ok(archive)
}

pub(crate) fn remote_unpack_source_command(remote_dir: &str) -> String {
    let source_dir = format!("{remote_dir}/source");
    format!(
        "set -eu; mkdir {}; tar -xzf {} -C {}",
        shell_quote(&source_dir),
        shell_quote(&format!("{remote_dir}/source.tar.gz")),
        shell_quote(&source_dir),
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

    /// Officials no longer vendor into the project (organization#951 WS4):
    /// `deploy_source` uploads only the source snapshot, and the remote
    /// `phoxal build` it runs materializes officials itself via `cargo
    /// install` - there is no second `artifacts.tar.gz` payload to unpack.
    #[test]
    fn source_unpack_extracts_only_the_source_payload() {
        let command = remote_unpack_source_command("/tmp/phoxal-deploy.ABC");
        assert!(command.contains("source.tar.gz"));
        assert!(command.contains("/tmp/phoxal-deploy.ABC/source"));
        assert!(!command.contains("artifacts"));
    }

    /// `archive_directory` (shared by the source payload and any future
    /// payload) must preserve symlinks verbatim - a git-tracked source tree
    /// can legitimately contain one.
    #[cfg(unix)]
    #[test]
    fn archive_directory_preserves_symlinks() -> Result<()> {
        let source = tempfile::tempdir()?;
        std::fs::create_dir_all(source.path().join("real"))?;
        std::fs::write(source.path().join("real/file"), b"ELF")?;
        std::os::unix::fs::symlink("real", source.path().join("linked"))?;

        let archive = archive_directory(source.path(), "phoxal-symlink-test-")?;
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

        let linked = extracted.path().join("linked");
        assert!(std::fs::symlink_metadata(&linked)?.file_type().is_symlink());
        assert_eq!(
            std::fs::read_link(&linked)?,
            std::path::PathBuf::from("real")
        );
        assert_eq!(std::fs::read(linked.join("file"))?, b"ELF");
        Ok(())
    }

    #[test]
    fn transfer_archives_disable_host_extended_metadata() {
        assert_eq!(ARCHIVE_TAR_OPTIONS, ["--no-xattrs", "-czf"]);
        assert_eq!(ARCHIVE_TAR_ENV, ("COPYFILE_DISABLE", "1"));
    }
}
