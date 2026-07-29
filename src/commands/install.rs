//! Install and roll back immutable compiled runtime releases.

use std::future::Future;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use clap::Args;
use sha2::{Digest, Sha256};

use crate::supervisor::{ProjectLock, ProjectLockIdentity, ProjectOperation};

const READINESS_TIMEOUT: Duration = Duration::from_secs(5 * 60);

#[derive(Debug, Args)]
pub struct Install {
    #[arg(value_name = "BUILD_PHOXAL")]
    archive: PathBuf,
}

#[derive(Debug, Args)]
pub struct Rollback {
    #[arg(long, value_name = "RELEASE_DIRECTORY_NAME")]
    to: Option<String>,
}

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

trait ServiceManager {
    fn stop(&self) -> Result<()>;
    fn start(&self) -> Result<()>;
    fn wait_ready<'a>(
        &'a self,
        supervisor_socket: &'a Path,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>>;
}

struct SystemdService;

impl ServiceManager for SystemdService {
    fn stop(&self) -> Result<()> {
        systemctl(["stop", phoxal_cli_project::SYSTEMD_UNIT])
    }

    fn start(&self) -> Result<()> {
        systemctl(["reset-failed", phoxal_cli_project::SYSTEMD_UNIT])?;
        systemctl(["start", "--no-block", phoxal_cli_project::SYSTEMD_UNIT])
    }

    fn wait_ready<'a>(
        &'a self,
        supervisor_socket: &'a Path,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>> {
        Box::pin(async move {
            let deadline = tokio::time::Instant::now() + READINESS_TIMEOUT;
            loop {
                if let Some(failure) = systemd_failure()? {
                    bail!("phoxal.service failed before readiness: {failure}");
                }
                if let Ok(client) =
                    phoxal_cli_client::SupervisorClient::connect(supervisor_socket).await
                {
                    match crate::run::required_readiness(&client.snapshots().current()) {
                        crate::run::Readiness::Ready => return Ok(()),
                        crate::run::Readiness::Failed(failures) => {
                            bail!(
                                "installed runtime failed readiness: {}",
                                failures.join(", ")
                            )
                        }
                        crate::run::Readiness::Pending => {}
                    }
                }
                if tokio::time::Instant::now() >= deadline {
                    bail!("timed out waiting for installed supervisor readiness");
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        })
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

impl Install {
    pub async fn run(&self, app: &crate::AppContext) -> Result<()> {
        require_system_installation()?;
        let archive = self.archive.canonicalize().with_context(|| {
            format!("failed to resolve build archive {}", self.archive.display())
        })?;
        let release = install_archive(
            &archive,
            &InstallRoots::system(),
            &SystemdService,
            app.offline,
        )
        .await?;
        app.ui
            .info(format!("installed runtime release {}", release.display()));
        Ok(())
    }
}

impl Rollback {
    pub async fn run(&self, app: &crate::AppContext) -> Result<()> {
        require_system_installation()?;
        let release =
            rollback_release(self.to.as_deref(), &InstallRoots::system(), &SystemdService).await?;
        app.ui
            .info(format!("active runtime restored to {}", release.display()));
        Ok(())
    }
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
    service: &dyn ServiceManager,
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

    let prepared = async {
        phoxal_cli_project::validate(phoxal_cli_project::ValidateRequest {
            source: phoxal_cli_project::ValidationSource::Archive(
                phoxal_cli_project::ArchiveValidation {
                    archive: archive.to_path_buf(),
                    destination: candidate.clone(),
                },
            ),
            offline,
            reporter: std::sync::Arc::new(phoxal_cli_project::SilentReporter),
        })
        .await?;
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
    let _lock = match ProjectLock::acquire_path(&roots.state.join("project.lock"), identity)
        .context("failed to acquire the installed-runtime lock")
    {
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
    if let Err(error) = service
        .wait_ready(&roots.volatile.join("supervisor.sock"))
        .await
    {
        restore_after_failed_activation(previous.as_deref(), roots, service).await?;
        discard_failed_release(&release, &roots.releases)?;
        return Err(error).context("new release was rolled back after failed readiness");
    }
    Ok(release)
}

async fn rollback_release(
    requested: Option<&str>,
    roots: &InstallRoots,
    service: &dyn ServiceManager,
) -> Result<PathBuf> {
    let active = active_release(&roots.active, &roots.releases)?
        .context("cannot roll back: /var/phoxal does not select a release")?;
    let selected = select_rollback_release(requested, &active, &roots.releases)?;
    service.stop().context("failed to stop phoxal.service")?;
    let identity = ProjectLockIdentity::resolve(&roots.active, ProjectOperation::Install);
    let _lock = ProjectLock::acquire_path(&roots.state.join("project.lock"), identity)
        .context("failed to acquire the installed-runtime lock")?;
    atomic_symlink_switch(&roots.active, &selected)?;
    drop(_lock);
    if let Err(error) = service.start().context("failed to start rollback release") {
        restore_after_failed_activation(Some(&active), roots, service).await?;
        return Err(error);
    }
    if let Err(error) = service
        .wait_ready(&roots.volatile.join("supervisor.sock"))
        .await
    {
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
    service: &dyn ServiceManager,
) -> Result<()> {
    let _ = service.stop();
    if let Some(previous) = previous {
        atomic_symlink_switch(&roots.active, previous)?;
        service
            .start()
            .context("failed to restart the previous release")?;
        service
            .wait_ready(&roots.volatile.join("supervisor.sock"))
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

fn sha256_file(path: &Path) -> Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct FakeService {
        operations: Mutex<Vec<&'static str>>,
        fail_readiness_once: Mutex<bool>,
    }

    impl ServiceManager for FakeService {
        fn stop(&self) -> Result<()> {
            self.operations.lock().unwrap().push("stop");
            Ok(())
        }

        fn start(&self) -> Result<()> {
            self.operations.lock().unwrap().push("start");
            Ok(())
        }

        fn wait_ready<'a>(
            &'a self,
            _supervisor_socket: &'a Path,
        ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>> {
            Box::pin(async move {
                self.operations.lock().unwrap().push("ready");
                let mut fail = self.fail_readiness_once.lock().unwrap();
                if *fail {
                    *fail = false;
                    bail!("forced readiness failure");
                }
                Ok(())
            })
        }
    }

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
        let service = FakeService::default();

        restore_after_failed_activation(Some(&previous), &roots, &service).await?;

        assert_eq!(std::fs::read_link(&roots.active)?, previous);
        assert_eq!(
            *service.operations.lock().unwrap(),
            ["stop", "start", "ready"]
        );
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
