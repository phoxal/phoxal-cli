//! Installing and rolling back a runtime release on a systemd host.
//!
//! Installation is the client's, never the daemon's: it
//! validates an archive, swaps the active-release symlink atomically, and
//! restarts the unit - all of which mutate a bundle a daemon may be executing.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};

use crate::cli::context::AppContext;
use crate::digest::sha256_file;
use crate::lock::{ProjectLock, ProjectLockIdentity, ProjectOperation};

/// The Zenoh endpoint an installed `phoxald` binds. It is derived from the
/// installed runtime layout, not asked for, so a client and the unit can never
/// disagree about where the execution answers.
fn installed_endpoint(roots: &InstallRoots) -> String {
    format!(
        "unixsock-stream/{}",
        roots.volatile.join("supervisor.sock").display()
    )
}

const READINESS_TIMEOUT: Duration = Duration::from_secs(5 * 60);

#[derive(Debug, Clone)]
struct InstallRoots {
    active: PathBuf,
    releases: PathBuf,
    state: PathBuf,
    volatile: PathBuf,
}

impl InstallRoots {
    fn system() -> Self {
        Self {
            active: PathBuf::from(phoxal_cli_project::ACTIVE_RUNTIME_ROOT),
            releases: PathBuf::from(phoxal_cli_project::RELEASES_ROOT),
            state: PathBuf::from(phoxal_cli_project::INSTALLED_STATE_ROOT),
            volatile: PathBuf::from(phoxal_cli_project::INSTALLED_VOLATILE_ROOT),
        }
    }
}

enum ServiceControl {
    Systemd,
    #[cfg(test)]
    Fake {
        operations: std::sync::Arc<std::sync::Mutex<Vec<&'static str>>>,
        fail_readiness_once: std::sync::Arc<std::sync::Mutex<bool>>,
    },
}

impl ServiceControl {
    fn stop(&self) -> Result<()> {
        match self {
            Self::Systemd => systemctl(["stop", phoxal_cli_project::SYSTEMD_UNIT]),
            #[cfg(test)]
            Self::Fake { operations, .. } => {
                operations.lock().unwrap().push("stop");
                Ok(())
            }
        }
    }

    fn start(&self) -> Result<()> {
        match self {
            Self::Systemd => {
                systemctl(["reset-failed", phoxal_cli_project::SYSTEMD_UNIT])?;
                systemctl(["start", "--no-block", phoxal_cli_project::SYSTEMD_UNIT])
            }
            #[cfg(test)]
            Self::Fake { operations, .. } => {
                operations.lock().unwrap().push("start");
                Ok(())
            }
        }
    }

    async fn wait_ready(&self, endpoint: &str) -> Result<()> {
        #[cfg(test)]
        if let Self::Fake {
            operations,
            fail_readiness_once,
        } = self
        {
            operations.lock().unwrap().push("ready");
            let mut fail = fail_readiness_once.lock().unwrap();
            if *fail {
                *fail = false;
                bail!("forced readiness failure");
            }
            return Ok(());
        }

        let deadline = tokio::time::Instant::now() + READINESS_TIMEOUT;
        loop {
            if let Some(failure) = systemd_failure()? {
                bail!("phoxal.service failed before readiness: {failure}");
            }
            // The completed handshake plus a snapshot is the readiness
            // signal, exactly as it is for an interactive `run`
            //. A unit that is up but whose graph never
            // came together answers connect and reports why.
            if let Ok(attachment) = phoxal_supervisor_client::Attachment::open(
                &phoxal_supervisor_client::AttachmentConfig::new(
                    endpoint,
                    crate::attach::CLIENT_PARTICIPANT,
                ),
            )
            .await
            {
                let readiness = tokio::time::timeout(
                    Duration::from_millis(250),
                    attachment.port().wait_ready(),
                )
                .await;
                let _ = attachment.close().await;
                if let Ok(readiness) = readiness {
                    readiness.context("the installed runtime failed readiness")?;
                    return Ok(());
                }
            }
            if tokio::time::Instant::now() >= deadline {
                bail!("timed out waiting for the installed supervisor to become ready");
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }
}

fn systemd_failure() -> Result<Option<String>> {
    let output = Command::new("systemctl")
        .args([
            "show",
            "--no-pager",
            "--property=ActiveState",
            "--property=NRestarts",
            "--property=Result",
            phoxal_cli_project::SYSTEMD_UNIT,
        ])
        .output()?;
    anyhow::ensure!(
        output.status.success(),
        "failed to inspect phoxal.service readiness state"
    );
    Ok(parse_systemd_failure(&String::from_utf8(output.stdout)?))
}

fn parse_systemd_failure(state: &str) -> Option<String> {
    let active = state
        .lines()
        .find_map(|line| line.strip_prefix("ActiveState="))
        .unwrap_or_default();
    let result = state
        .lines()
        .find_map(|line| line.strip_prefix("Result="))
        .unwrap_or_default();
    let restarts = state
        .lines()
        .find_map(|line| line.strip_prefix("NRestarts="))
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or_default();
    if active == "failed" || !matches!(result, "" | "success") || restarts > 0 {
        Some(format!(
            "ActiveState={active}, Result={result}, NRestarts={restarts}"
        ))
    } else {
        None
    }
}

pub(crate) async fn install(archive: &Path, offline: bool) -> Result<PathBuf> {
    require_system_installation()?;
    let archive = archive
        .canonicalize()
        .with_context(|| format!("failed to resolve build archive {}", archive.display()))?;
    install_archive(
        &archive,
        &InstallRoots::system(),
        &ServiceControl::Systemd,
        offline,
    )
    .await
}

pub(crate) async fn rollback(release: Option<&str>) -> Result<PathBuf> {
    require_system_installation()?;
    rollback_release(release, &InstallRoots::system(), &ServiceControl::Systemd).await
}

fn require_system_installation() -> Result<()> {
    anyhow::ensure!(
        unsafe { libc::geteuid() } == 0,
        "`phoxal install` and `phoxal rollback` require root"
    );
    anyhow::ensure!(
        Path::new(phoxal_cli_project::SYSTEMD_ACTIVE_ROOT).is_dir(),
        "systemd is not the active service manager on this host"
    );
    anyhow::ensure!(
        Path::new(phoxal_cli_project::SYSTEMD_UNIT_PATH).is_file(),
        "phoxal.service is not installed; run `sudo phoxal service install` first"
    );
    Ok(())
}

async fn install_archive(
    archive: &Path,
    roots: &InstallRoots,
    service: &ServiceControl,
    offline: bool,
) -> Result<PathBuf> {
    require_build_archive(archive)?;
    let digest = sha256_file(archive)?;
    let name = format!(
        "{}-{}",
        sortable_utc_timestamp(SystemTime::now())?,
        &digest[..8]
    );
    std::fs::create_dir_all(&roots.releases)?;
    std::fs::create_dir_all(&roots.state)?;
    std::fs::create_dir_all(&roots.volatile)?;
    let candidate = roots.releases.join(format!(".{name}.candidate"));
    let release = roots.releases.join(&name);
    anyhow::ensure!(
        !release.exists(),
        "release {name} already exists; retry after the clock advances"
    );
    remove_dir_if_present(&candidate)?;

    let validation_archive = archive.to_path_buf();
    let validation_destination = candidate.clone();
    let prepared = async {
        tokio::task::spawn_blocking(move || {
            phoxal_cli_project::validate(phoxal_cli_project::ValidateRequest {
                source: phoxal_cli_project::ValidationSource::Archive(
                    phoxal_cli_project::ArchiveValidation {
                        archive: validation_archive,
                        destination: validation_destination,
                    },
                ),
                offline,
                reporter: std::sync::Arc::new(phoxal_cli_project::SilentReporter),
            })
        })
        .await??;
        reject_simulation_bundle(&candidate)?;
        fsync_tree(&candidate)?;
        Ok::<_, anyhow::Error>(())
    }
    .await;
    if let Err(error) = prepared {
        remove_dir_if_present(&candidate)?;
        return Err(error);
    }
    let pre_stop_active = active_release(&roots.active, &roots.releases)?;
    if let Err(error) = service.stop().context("failed to stop phoxal.service") {
        remove_dir_if_present(&candidate)?;
        return Err(error);
    }
    let identity = ProjectLockIdentity::resolve(&roots.active, ProjectOperation::Install);
    // The service was asked to stop above; the supervisor lock is what proves
    // it actually let go. Activating a release switches the symlink the live
    // execution resolves its bundle through, so a daemon still holding on is a
    // refusal rather than a race.
    let _lock = match ProjectLock::acquire_path(&roots.state.join("project.lock"), identity)
        .context("failed to acquire the installed-runtime lock")
        .and_then(|lock| {
            crate::lock::refuse_while_execution_is_live(&roots.active)?;
            Ok(lock)
        }) {
        Ok(lock) => lock,
        Err(error) => {
            remove_dir_if_present(&candidate)?;
            if pre_stop_active.is_some() {
                let _ = service.start();
            }
            return Err(error);
        }
    };
    let previous = active_release(&roots.active, &roots.releases)?;
    if let Err(error) = (|| -> Result<()> {
        std::fs::rename(&candidate, &release)?;
        fsync_dir(&roots.releases)?;
        atomic_symlink_switch(&roots.active, &release)?;
        Ok(())
    })() {
        drop(_lock);
        remove_dir_if_present(&candidate)?;
        restore_after_failed_activation(previous.as_deref(), roots, service).await?;
        discard_failed_release(&release, &roots.releases)?;
        return Err(error);
    }
    drop(_lock);

    if let Err(error) = service.start().context("failed to start phoxal.service") {
        restore_after_failed_activation(previous.as_deref(), roots, service).await?;
        discard_failed_release(&release, &roots.releases)?;
        return Err(error);
    }
    if let Err(error) = service.wait_ready(&installed_endpoint(roots)).await {
        restore_after_failed_activation(previous.as_deref(), roots, service).await?;
        discard_failed_release(&release, &roots.releases)?;
        return Err(error).context("new release was rolled back after failed readiness");
    }
    Ok(release)
}

async fn rollback_release(
    requested: Option<&str>,
    roots: &InstallRoots,
    service: &ServiceControl,
) -> Result<PathBuf> {
    let active = active_release(&roots.active, &roots.releases)?
        .context("cannot roll back: /var/phoxal does not select a release")?;
    let selected = select_rollback_release(requested, &active, &roots.releases)?;
    service.stop().context("failed to stop phoxal.service")?;
    let identity = ProjectLockIdentity::resolve(&roots.active, ProjectOperation::Install);
    let _lock = ProjectLock::acquire_path(&roots.state.join("project.lock"), identity)
        .context("failed to acquire the installed-runtime lock")?;
    crate::lock::refuse_while_execution_is_live(&roots.active)?;
    atomic_symlink_switch(&roots.active, &selected)?;
    drop(_lock);
    if let Err(error) = service.start().context("failed to start rollback release") {
        restore_after_failed_activation(Some(&active), roots, service).await?;
        return Err(error);
    }
    if let Err(error) = service.wait_ready(&installed_endpoint(roots)).await {
        restore_after_failed_activation(Some(&active), roots, service).await?;
        return Err(error).context("rollback target failed readiness; original restored");
    }
    Ok(selected)
}

fn discard_failed_release(release: &Path, releases: &Path) -> Result<()> {
    remove_dir_if_present(release)?;
    fsync_dir(releases)
}

async fn restore_after_failed_activation(
    previous: Option<&Path>,
    roots: &InstallRoots,
    service: &ServiceControl,
) -> Result<()> {
    let _ = service.stop();
    if let Some(previous) = previous {
        atomic_symlink_switch(&roots.active, previous)?;
        service
            .start()
            .context("failed to restart the previous release")?;
        service
            .wait_ready(&installed_endpoint(roots))
            .await
            .context("previous release did not recover after rollback")?;
    } else {
        remove_file_if_present(&roots.active)?;
        fsync_dir(
            roots
                .active
                .parent()
                .context("active runtime path has no parent")?,
        )?;
    }
    Ok(())
}

/// The error a simulation bundle earns at the installer.
pub(crate) const SIMULATION_BUNDLE_REJECTED: &str = "PHOXAL-E-INSTALL-SIMULATION-BUNDLE";

/// Refuse to install a simulation bundle.
///
/// Keeping simulation off systemd is an install-path rule, not a daemon rule
///: `phoxald` reads `clock` from the manifest like any other
/// bundle and would come up waiting for a world clock only the client-owned
/// Webots can produce - forever, on a `Restart=on-failure` unit. So the refusal
/// is here, where the durable install is being made.
fn reject_simulation_bundle(root: &Path) -> Result<()> {
    let bundle = phoxal_bundle::RuntimeBundle::open_verified(root)
        .context("failed to verify the runtime bundle before installation")?;
    ensure_real_clock(bundle.robot().clock())
}

/// The clock rule alone, so the refusal is testable without a bundle on disk.
fn ensure_real_clock(clock: phoxal_model::Clock) -> Result<()> {
    if clock == phoxal_model::Clock::Simulated {
        bail!(
            "error[{SIMULATION_BUNDLE_REJECTED}]: this build.phoxal is a simulation bundle \
             (clock: simulated) and is never installed. A simulated execution needs the \
             client-owned Webots for its world clock, which a systemd service has no way to \
             start; run it with `phoxal simulation webots run <ROBOT_YAML> <WORLD>` instead, and \
             install a real-clock bundle built by `phoxal build`"
        );
    }
    Ok(())
}

fn require_build_archive(path: &Path) -> Result<()> {
    anyhow::ensure!(path.is_file(), "{} is not a file", path.display());
    anyhow::ensure!(
        path.file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.ends_with(".build.phoxal") || name == "build.phoxal"),
        "{} is not a build.phoxal archive",
        path.display()
    );
    Ok(())
}

fn active_release(active: &Path, releases: &Path) -> Result<Option<PathBuf>> {
    match std::fs::symlink_metadata(active) {
        Ok(metadata) => {
            anyhow::ensure!(
                metadata.file_type().is_symlink(),
                "{} exists but is not a symlink",
                active.display()
            );
            let target = std::fs::read_link(active).map(|target| {
                if target.is_absolute() {
                    target
                } else {
                    active
                        .parent()
                        .unwrap_or_else(|| Path::new("/"))
                        .join(target)
                }
            })?;
            let canonical = target.canonicalize()?;
            let canonical_releases = releases.canonicalize()?;
            anyhow::ensure!(
                canonical.parent() == Some(canonical_releases.as_path()),
                "{} points outside {}",
                active.display(),
                releases.display()
            );
            Ok(Some(canonical))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn select_rollback_release(
    requested: Option<&str>,
    active: &Path,
    releases: &Path,
) -> Result<PathBuf> {
    let mut names = std::fs::read_dir(releases)?
        .filter_map(std::result::Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| valid_release_name(name))
        .collect::<Vec<_>>();
    names.sort();
    let selected = if let Some(requested) = requested {
        anyhow::ensure!(
            valid_release_name(requested),
            "invalid release directory name `{requested}`"
        );
        anyhow::ensure!(
            names.iter().any(|name| name == requested),
            "release `{requested}` does not exist"
        );
        requested.to_string()
    } else {
        let active_name = active
            .file_name()
            .and_then(|name| name.to_str())
            .context("active release has no valid directory name")?;
        let index = names
            .iter()
            .position(|name| name == active_name)
            .context("active release is not in the release index")?;
        anyhow::ensure!(index > 0, "there is no older release to roll back to");
        names[index - 1].clone()
    };
    Ok(releases.join(selected))
}

fn valid_release_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    bytes.len() == 29
        && bytes[8] == b'T'
        && bytes[15] == b'.'
        && bytes[19] == b'Z'
        && bytes[20] == b'-'
        && bytes[..8].iter().all(u8::is_ascii_digit)
        && bytes[9..15].iter().all(u8::is_ascii_digit)
        && bytes[16..19].iter().all(u8::is_ascii_digit)
        && bytes[21..].iter().all(u8::is_ascii_hexdigit)
}

fn sortable_utc_timestamp(now: SystemTime) -> Result<String> {
    let duration = now.duration_since(UNIX_EPOCH)?;
    let seconds: libc::time_t = duration.as_secs().try_into()?;
    let mut broken_down = std::mem::MaybeUninit::<libc::tm>::uninit();
    // SAFETY: both pointers refer to initialized, properly aligned storage and
    // `gmtime_r` writes one `tm` without retaining either pointer.
    let result = unsafe { libc::gmtime_r(&seconds, broken_down.as_mut_ptr()) };
    anyhow::ensure!(!result.is_null(), "failed to convert current time to UTC");
    // SAFETY: a non-null `gmtime_r` result initialized the output `tm`.
    let tm = unsafe { broken_down.assume_init() };
    Ok(format!(
        "{:04}{:02}{:02}T{:02}{:02}{:02}.{:03}Z",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec,
        duration.subsec_millis()
    ))
}

fn atomic_symlink_switch(active: &Path, target: &Path) -> Result<()> {
    let parent = active
        .parent()
        .context("active runtime path has no parent directory")?;
    std::fs::create_dir_all(parent)?;
    let candidate = parent.join(format!(".phoxal-link-{}", std::process::id()));
    remove_file_if_present(&candidate)?;
    std::os::unix::fs::symlink(target, &candidate)?;
    std::fs::rename(&candidate, active)?;
    fsync_dir(parent)
}

fn fsync_tree(root: &Path) -> Result<()> {
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        let metadata = std::fs::symlink_metadata(&path)?;
        if metadata.is_dir() {
            fsync_tree(&path)?;
        } else if metadata.is_file() {
            std::fs::File::open(&path)?.sync_all()?;
        }
    }
    fsync_dir(root)
}

fn fsync_dir(path: &Path) -> Result<()> {
    std::fs::File::open(path)?.sync_all()?;
    Ok(())
}

fn remove_file_if_present(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn remove_dir_if_present(path: &Path) -> Result<()> {
    match std::fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn systemctl<const N: usize>(args: [&str; N]) -> Result<()> {
    let status = Command::new("systemctl").args(args).status()?;
    anyhow::ensure!(status.success(), "systemctl failed with {status}");
    Ok(())
}

pub(crate) async fn install_command(app: &AppContext, archive: &Path) -> Result<()> {
    let release = install(archive, app.offline).await?;
    app.ui
        .info(format!("installed runtime release {}", release.display()));
    Ok(())
}

pub(crate) async fn rollback_command(app: &AppContext, release: Option<&str>) -> Result<()> {
    let release = rollback(release).await?;
    app.ui
        .info(format!("active runtime restored to {}", release.display()));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn roots(temp: &tempfile::TempDir) -> InstallRoots {
        InstallRoots {
            active: temp.path().join("var/phoxal"),
            releases: temp.path().join("var/lib/phoxal/releases"),
            state: temp.path().join("var/lib/phoxal/state"),
            volatile: temp.path().join("run/phoxal"),
        }
    }

    #[test]
    fn release_names_are_sortable_and_strict() -> Result<()> {
        let timestamp =
            sortable_utc_timestamp(UNIX_EPOCH + Duration::from_millis(1_753_402_123_456))?;
        let name = format!("{timestamp}-deadbeef");
        assert!(valid_release_name(&name), "{name}");
        assert!(!valid_release_name("../deadbeef"));
        Ok(())
    }

    #[test]
    fn systemd_failure_state_is_detected_without_waiting_for_start_timeout() {
        assert!(
            parse_systemd_failure("ActiveState=activating\nResult=success\nNRestarts=0\n")
                .is_none()
        );
        assert_eq!(
            parse_systemd_failure("ActiveState=activating\nResult=exit-code\nNRestarts=0\n")
                .as_deref(),
            Some("ActiveState=activating, Result=exit-code, NRestarts=0")
        );
        assert_eq!(
            parse_systemd_failure("ActiveState=activating\nResult=success\nNRestarts=1\n")
                .as_deref(),
            Some("ActiveState=activating, Result=success, NRestarts=1")
        );
    }

    #[test]
    fn default_rollback_selects_immediately_older_release() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let roots = roots(&temp);
        std::fs::create_dir_all(&roots.releases)?;
        let older = roots.releases.join("20260724T010000.000Z-11111111");
        let active = roots.releases.join("20260725T010000.000Z-22222222");
        std::fs::create_dir(&older)?;
        std::fs::create_dir(&active)?;
        assert_eq!(
            select_rollback_release(None, &active, &roots.releases)?,
            older
        );
        Ok(())
    }

    #[tokio::test]
    async fn failed_activation_restores_the_previous_symlink_and_readiness() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let roots = roots(&temp);
        std::fs::create_dir_all(&roots.releases)?;
        std::fs::create_dir_all(&roots.state)?;
        std::fs::create_dir_all(&roots.volatile)?;
        let previous = roots.releases.join("20260724T010000.000Z-11111111");
        let failed = roots.releases.join("20260725T010000.000Z-22222222");
        std::fs::create_dir(&previous)?;
        std::fs::create_dir(&failed)?;
        atomic_symlink_switch(&roots.active, &failed)?;
        let operations = std::sync::Arc::new(Mutex::new(Vec::new()));
        let service = ServiceControl::Fake {
            operations: operations.clone(),
            fail_readiness_once: std::sync::Arc::new(Mutex::new(false)),
        };

        restore_after_failed_activation(Some(&previous), &roots, &service).await?;

        assert_eq!(std::fs::read_link(&roots.active)?, previous);
        assert_eq!(*operations.lock().unwrap(), ["stop", "start", "ready"]);
        Ok(())
    }

    #[test]
    fn atomic_switch_never_exposes_a_partial_release() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let roots = roots(&temp);
        std::fs::create_dir_all(&roots.releases)?;
        let first = roots.releases.join("20260724T010000.000Z-11111111");
        let second = roots.releases.join("20260725T010000.000Z-22222222");
        std::fs::create_dir(&first)?;
        std::fs::create_dir(&second)?;
        atomic_symlink_switch(&roots.active, &first)?;
        assert_eq!(std::fs::read_link(&roots.active)?, first);
        atomic_symlink_switch(&roots.active, &second)?;
        assert_eq!(std::fs::read_link(&roots.active)?, second);
        Ok(())
    }

    #[test]
    fn post_activation_power_loss_state_remains_explicitly_rollbackable_without_metadata()
    -> Result<()> {
        let temp = tempfile::tempdir()?;
        let roots = roots(&temp);
        std::fs::create_dir_all(&roots.releases)?;
        let previous = roots.releases.join("20260724T010000.000Z-11111111");
        let active = roots.releases.join("20260725T010000.000Z-22222222");
        std::fs::create_dir(&previous)?;
        std::fs::create_dir(&active)?;

        // This is the documented narrow crash window: activation completed,
        // but the process vanished before it could confirm readiness.
        atomic_symlink_switch(&roots.active, &active)?;
        assert!(!roots.state.join("installed.json").exists());
        assert!(!roots.state.join("previous.json").exists());

        let selected = select_rollback_release(None, &active, &roots.releases)?;
        assert_eq!(selected, previous);
        atomic_symlink_switch(&roots.active, &selected)?;
        assert_eq!(std::fs::read_link(&roots.active)?, previous);
        Ok(())
    }

    /// A simulation bundle is never installed, and the refusal says why and
    /// what to run instead.
    #[test]
    fn a_simulation_bundle_is_rejected_at_the_installer_with_a_named_error() {
        use phoxal_model::Clock;

        assert!(ensure_real_clock(Clock::Real).is_ok());
        let error = ensure_real_clock(Clock::Simulated)
            .expect_err("a simulated bundle is never installable")
            .to_string();
        assert!(error.contains(SIMULATION_BUNDLE_REJECTED), "{error}");
        assert!(error.contains("clock: simulated"), "{error}");
        assert!(error.contains("phoxal simulation webots run"), "{error}");
    }

    #[test]
    fn failed_new_release_is_not_left_in_the_rollback_index() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let roots = roots(&temp);
        std::fs::create_dir_all(&roots.releases)?;
        let failed = roots.releases.join("20260725T010000.000Z-22222222");
        std::fs::create_dir(&failed)?;
        discard_failed_release(&failed, &roots.releases)?;
        assert!(!failed.exists());
        Ok(())
    }
}
