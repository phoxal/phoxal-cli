//! Installing and rolling back a deployment release on a systemd host.
//!
//! Installation is the client's, never the supervisor's: it validates an archive,
//! swaps the active-release symlink atomically, and restarts the unit - all of
//! which mutate a release a supervisor may be executing.
//!
//! A release is one directory holding the supervisor and the bundle it runs, and
//! `/var/phoxal` selects exactly one of them. Activation and rollback are that
//! one symlink swap, so the supervisor and the bundle move together: there is no
//! moment, and no failure path, at which one release's supervisor could be
//! executing another release's bundle.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};

use phoxal_cli_project::BUNDLE_DIR;

use super::units;
use crate::cli::context::AppContext;
use crate::lock::{ProjectLock, ProjectLockIdentity, ProjectOperation};

/// The Zenoh endpoint an installed framework supervisor binds. It is derived from the
/// installed runtime paths, not asked for, so a client and the unit can never
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
    /// Where the per-runtime units this install generates are written.
    units: PathBuf,
}

impl InstallRoots {
    fn system() -> Self {
        Self {
            active: PathBuf::from(phoxal_cli_project::ACTIVE_RUNTIME_ROOT),
            releases: PathBuf::from(phoxal_cli_project::RELEASES_ROOT),
            state: PathBuf::from(phoxal_cli_project::INSTALLED_STATE_ROOT),
            volatile: PathBuf::from(phoxal_cli_project::INSTALLED_VOLATILE_ROOT),
            units: PathBuf::from(phoxal_cli_project::SYSTEMD_UNIT_ROOT),
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
    /// Stop the whole robot.
    ///
    /// Every runtime unit is `PartOf=` the supervisor's, so stopping the
    /// supervisor stops them with it; the target is stopped too so systemd does
    /// not consider the robot still wanted.
    fn stop(&self) -> Result<()> {
        match self {
            Self::Systemd => {
                let _ = systemctl(["stop", crate::application::units::TARGET_UNIT]);
                systemctl(["stop", phoxal_cli_host::paths::SYSTEMD_UNIT])
            }
            #[cfg(test)]
            Self::Fake { operations, .. } => {
                operations.lock().unwrap().push("stop");
                Ok(())
            }
        }
    }

    fn start(&self) -> Result<()> {
        match self {
            // The supervisor first, then the runtimes the target wants: a
            // runtime that starts before the router exists would spend its
            // first seconds failing to connect for no reason.
            Self::Systemd => {
                systemctl(["reset-failed", phoxal_cli_host::paths::SYSTEMD_UNIT])?;
                systemctl(["start", "--no-block", phoxal_cli_host::paths::SYSTEMD_UNIT])?;
                systemctl([
                    "start",
                    "--no-block",
                    crate::application::units::TARGET_UNIT,
                ])
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
                bail!("phoxal-supervisor.service failed before readiness: {failure}");
            }
            // The completed handshake plus a snapshot is the readiness
            // signal, exactly as it is for an interactive `run`. A unit that is
            // up but whose graph never came together answers connect and
            // reports why.
            let connection = phoxal_client::Connection::connect(
                &phoxal_client::ConnectOptions::new(endpoint, crate::attach::CLIENT_PARTICIPANT),
            )
            .await;
            match connection {
                Ok(connection) => {
                    let client = connection.client();
                    let readiness =
                        tokio::time::timeout(Duration::from_millis(250), client.wait_ready()).await;
                    let _ = connection.close().await;
                    if let Ok(readiness) = readiness {
                        readiness.context("the installed runtime failed readiness")?;
                        return Ok(());
                    }
                }
                // The installed unit answered and speaks another framework
                // train. Waiting out the readiness budget would report a
                // timeout for something that will never become ready.
                Err(error) if error.is_compatibility_refusal() => return Err(error.into()),
                Err(_) => {}
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
            phoxal_cli_host::paths::SYSTEMD_UNIT,
        ])
        .output()?;
    anyhow::ensure!(
        output.status.success(),
        "failed to inspect phoxal-supervisor.service readiness state"
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
        Path::new(phoxal_cli_host::paths::SYSTEMD_UNIT_PATH).is_file(),
        "phoxal-supervisor.service is not installed; run `sudo phoxal service install` first"
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
    // The archive's own sidecar is the only integrity fence in the system: a
    // bundle records no digest of anything, so an archive nobody attested to
    // is not installed. A missing sidecar is refused for the same reason a
    // mismatched one is.
    let digest = phoxal_cli_project::verify_against_digest_sidecar(archive)?;
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
        fsync_tree(&candidate)?;
        Ok::<_, anyhow::Error>(())
    }
    .await;
    if let Err(error) = prepared {
        remove_dir_if_present(&candidate)?;
        return Err(error);
    }
    let pre_stop_active = active_release(&roots.active)?;
    if let Err(error) = service
        .stop()
        .context("failed to stop phoxal-supervisor.service")
    {
        remove_dir_if_present(&candidate)?;
        return Err(error);
    }
    let identity = ProjectLockIdentity::resolve(&roots.active, ProjectOperation::Install);
    // The service was asked to stop above; the supervisor lock is what proves
    // it actually let go. Activating a release switches the symlink the live
    // execution resolves its bundle through, so a supervisor still holding on is a
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
    let previous = active_release(&roots.active)?;
    if let Err(error) = (|| -> Result<()> {
        std::fs::rename(&candidate, &release)?;
        fsync_dir(&roots.releases)?;
        atomic_symlink_switch(&roots.active, &release)?;
        // The robot's process set is a property of the bundle now active, so
        // the units are regenerated with the symlink rather than left to
        // describe the release that was there before.
        regenerate_runtime_units(&release, &roots.units)?;
        Ok(())
    })() {
        drop(_lock);
        remove_dir_if_present(&candidate)?;
        restore_after_failed_activation(previous.as_deref(), roots, service).await?;
        discard_failed_release(&release, &roots.releases)?;
        return Err(error);
    }
    drop(_lock);

    if let Err(error) = service
        .start()
        .context("failed to start phoxal-supervisor.service")
    {
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
    let active = active_release(&roots.active)?
        .context("cannot roll back: /var/phoxal does not select a release")?;
    let selected = select_rollback_release(requested, &active, &roots.releases)?;
    service
        .stop()
        .context("failed to stop phoxal-supervisor.service")?;
    let identity = ProjectLockIdentity::resolve(&roots.active, ProjectOperation::Install);
    let _lock = ProjectLock::acquire_path(&roots.state.join("project.lock"), identity)
        .context("failed to acquire the installed-runtime lock")?;
    crate::lock::refuse_while_execution_is_live(&roots.active)?;
    atomic_symlink_switch(&roots.active, &selected)?;
    // A rollback activates a different bundle, which may declare a different
    // process set - so its units are regenerated exactly as an install's are.
    regenerate_runtime_units(&selected, &roots.units)?;
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
        regenerate_runtime_units(previous, &roots.units)?;
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

/// Write one systemd unit per runtime the installed bundle declares, plus the
/// target that owns them, and remove the ones a previous bundle needed and this
/// one does not.
///
/// A robot's process set is a property of its manifest, so it is regenerated
/// whenever the active release changes - by an install and by a rollback alike.
/// A stale unit left behind would be systemd faithfully restarting a runtime
/// the robot no longer has.
fn regenerate_runtime_units(release: &Path, unit_root: &Path) -> Result<()> {
    let runtimes = phoxal_cli_project::bundle_runtimes(&release.join(BUNDLE_DIR))?;
    let generated = units::bundle_units(&runtimes);
    std::fs::create_dir_all(unit_root)?;
    for (name, contents) in &generated {
        let path = unit_root.join(name);
        write_managed_unit(&path, contents)?;
    }
    let keep = generated
        .iter()
        .map(|(name, _)| name.clone())
        .collect::<std::collections::BTreeSet<_>>();
    for entry in std::fs::read_dir(unit_root)? {
        let entry = entry?;
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        if !units::is_generated_unit(&name) || keep.contains(&name) {
            continue;
        }
        let path = entry.path();
        // Only this CLI's own units are removed. A hand-written
        // `phoxal-something.service` belongs to whoever wrote it.
        if std::fs::read_to_string(&path)
            .is_ok_and(|contents| contents.lines().next() == Some(units::UNIT_MARKER))
        {
            std::fs::remove_file(&path)?;
        }
    }
    fsync_dir(unit_root)?;
    // systemd reads units once and caches them, so a unit written but not
    // reloaded does not exist as far as `systemctl start` is concerned. On a
    // first install that is every runtime unit and the target: the supervisor
    // would come up alone, nothing would join its graph, and readiness would
    // time out on a robot whose units were all written correctly. The reload is
    // part of writing them, not a separate step a caller may forget.
    reload_systemd()
}

/// Make systemd read the units this install just wrote.
///
/// Absent on a host without systemd - unit files are still written there, which
/// is what a container image or a hand-managed device wants, and nothing to
/// reload is not a failure.
fn reload_systemd() -> Result<()> {
    let Ok(output) = Command::new("systemctl").arg("daemon-reload").output() else {
        return Ok(());
    };
    anyhow::ensure!(
        output.status.success(),
        "systemctl daemon-reload failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(())
}

/// Write one unit, refusing to overwrite a file this CLI did not write.
fn write_managed_unit(path: &Path, contents: &str) -> Result<()> {
    if path.exists() {
        let current = std::fs::read_to_string(path)?;
        anyhow::ensure!(
            current.lines().next() == Some(units::UNIT_MARKER),
            "refusing to overwrite foreign unit {}",
            path.display()
        );
        if current == contents {
            return Ok(());
        }
    }
    let candidate = path.with_extension(format!("candidate-{}", std::process::id()));
    std::fs::write(&candidate, contents)?;
    std::fs::File::open(&candidate)?.sync_all()?;
    std::fs::rename(&candidate, path)?;
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

/// The release directory `active` currently selects, or `None` when nothing is
/// installed yet.
///
/// `active` is a symlink into the releases root by construction; anything else
/// there is a hand-mangled installation this command will not reason about.
fn active_release(active: &Path) -> Result<Option<PathBuf>> {
    match std::fs::symlink_metadata(active) {
        Ok(metadata) => {
            anyhow::ensure!(
                metadata.file_type().is_symlink(),
                "{} exists but is not a symlink",
                active.display()
            );
            let target = std::fs::read_link(active)?;
            Ok(Some(if target.is_absolute() {
                target
            } else {
                active
                    .parent()
                    .unwrap_or_else(|| Path::new("/"))
                    .join(target)
            }))
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
    app.ui.info(active_release_report("installed", &release));
    Ok(())
}

pub(crate) async fn rollback_command(app: &AppContext, release: Option<&str>) -> Result<()> {
    let release = rollback(release).await?;
    app.ui.info(active_release_report(
        "active release restored to",
        &release,
    ));
    Ok(())
}

/// What an activation reports: the release, and both halves that came with it.
///
/// Naming the supervisor beside the bundle is the point of the report - an
/// operator can see that the supervisor changed with the robot, which is exactly
/// the property a release exists to provide.
fn active_release_report(verb: &str, release: &Path) -> String {
    format!(
        "{verb} deployment release {}: supervisor {} and bundle {}",
        release.display(),
        release.join(phoxal_cli_project::SUPERVISOR_FILE).display(),
        release.join(BUNDLE_DIR).display(),
    )
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
            units: temp.path().join("etc/systemd/system"),
        }
    }

    /// Write a release whose two halves are both stamped with `tag`, so a test
    /// can tell which release the active link resolves to - and, reading them
    /// separately, prove they never come from different ones.
    fn release_stamped(roots: &InstallRoots, name: &str, tag: &str) -> Result<PathBuf> {
        let release = roots.releases.join(name);
        std::fs::create_dir_all(release.join(BUNDLE_DIR))?;
        std::fs::write(
            release.join(phoxal_cli_project::SUPERVISOR_FILE),
            format!("phoxal-supervisor of {tag}"),
        )?;
        // A real manifest, because activating a release regenerates the units
        // its process set implies - and that reads this document.
        let robot = phoxal_model::builder::RobotBuilder::new(tag)
            .service("drive", None)
            .build()
            .expect("a valid canonical robot");
        std::fs::write(
            release.join(BUNDLE_DIR).join(phoxal_bundle::MANIFEST_FILE),
            serde_json::to_vec_pretty(&phoxal_model::manifest::ManifestDocument::new(robot))?,
        )?;
        Ok(release)
    }

    /// Which release the active link resolves to, read off the manifest the
    /// unit generator also reads.
    fn active_bundle_tag(roots: &InstallRoots) -> Result<String> {
        let active = active_release(&roots.active)?.context("nothing is active")?;
        let bundle = phoxal_bundle::RuntimeBundle::open(active.join(BUNDLE_DIR))?;
        Ok(bundle.robot_id().to_string())
    }

    /// What the active link resolves to right now, read as two independent
    /// facts: which release the supervisor came from, and which release the
    /// bundle came from.
    fn active_halves(roots: &InstallRoots) -> Result<(String, String)> {
        let active = active_release(&roots.active)?.context("nothing is active")?;
        let supervisor = std::fs::read_to_string(active.join(phoxal_cli_project::SUPERVISOR_FILE))?;
        let _ = &roots.units;
        Ok((
            supervisor
                .strip_prefix("phoxal-supervisor of ")
                .unwrap_or(&supervisor)
                .to_string(),
            active_bundle_tag(roots)?,
        ))
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
        let previous = release_stamped(&roots, "20260724T010000.000Z-11111111", "previous")?;
        let failed = release_stamped(&roots, "20260725T010000.000Z-22222222", "failed")?;
        atomic_symlink_switch(&roots.active, &failed)?;
        let operations = std::sync::Arc::new(Mutex::new(Vec::new()));
        let service = ServiceControl::Fake {
            operations: operations.clone(),
            fail_readiness_once: std::sync::Arc::new(Mutex::new(false)),
        };

        restore_after_failed_activation(Some(&previous), &roots, &service).await?;

        assert_eq!(std::fs::read_link(&roots.active)?, previous);
        assert_eq!(*operations.lock().unwrap(), ["stop", "start", "ready"]);
        // The restored release's own process set is what systemd is left
        // describing, not the failed one's.
        assert!(
            roots
                .units
                .join(crate::application::units::runtime_unit_name("drive"))
                .is_file()
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

    /// The invariant this whole design exists for: a supervisor from one release
    /// can never be active while another release's bundle is. Every transition
    /// an operator can reach - activation, a failed activation restoring the
    /// previous release, an explicit rollback, and re-activating the newer one -
    /// is walked here, reading the two halves separately each time.
    #[tokio::test]
    async fn the_supervisor_and_the_bundle_never_come_from_different_releases() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let roots = roots(&temp);
        std::fs::create_dir_all(&roots.releases)?;
        let older = release_stamped(&roots, "20260724T010000.000Z-11111111", "older")?;
        let newer = release_stamped(&roots, "20260725T010000.000Z-22222222", "newer")?;
        let service = ServiceControl::Fake {
            operations: std::sync::Arc::new(Mutex::new(Vec::new())),
            fail_readiness_once: std::sync::Arc::new(Mutex::new(false)),
        };

        atomic_symlink_switch(&roots.active, &older)?;
        assert_eq!(
            active_halves(&roots)?,
            ("older".to_string(), "older".to_string())
        );

        // Activating the newer release moves both halves at once.
        atomic_symlink_switch(&roots.active, &newer)?;
        assert_eq!(
            active_halves(&roots)?,
            ("newer".to_string(), "newer".to_string())
        );

        // A failed activation restores the previous release whole, so the
        // supervisor goes back with the bundle rather than outliving it.
        restore_after_failed_activation(Some(&older), &roots, &service).await?;
        assert_eq!(
            active_halves(&roots)?,
            ("older".to_string(), "older".to_string())
        );

        // An explicit rollback selects a release, never a bundle.
        atomic_symlink_switch(&roots.active, &newer)?;
        let selected = select_rollback_release(None, &newer, &roots.releases)?;
        atomic_symlink_switch(&roots.active, &selected)?;
        assert_eq!(
            active_halves(&roots)?,
            ("older".to_string(), "older".to_string())
        );

        // And re-activating forward is the same single switch.
        atomic_symlink_switch(&roots.active, &newer)?;
        assert_eq!(
            active_halves(&roots)?,
            ("newer".to_string(), "newer".to_string())
        );
        Ok(())
    }

    /// A readiness failure restores the previous release complete: the supervisor
    /// flips back together with the bundle, and the service is restarted on it.
    #[tokio::test]
    async fn a_release_that_fails_readiness_is_replaced_by_the_previous_one_whole() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let roots = roots(&temp);
        std::fs::create_dir_all(&roots.releases)?;
        std::fs::create_dir_all(&roots.state)?;
        std::fs::create_dir_all(&roots.volatile)?;
        let previous = release_stamped(&roots, "20260724T010000.000Z-11111111", "previous")?;
        let failed = release_stamped(&roots, "20260725T010000.000Z-22222222", "failed")?;
        atomic_symlink_switch(&roots.active, &failed)?;
        let operations = std::sync::Arc::new(Mutex::new(Vec::new()));
        let service = ServiceControl::Fake {
            operations: operations.clone(),
            fail_readiness_once: std::sync::Arc::new(Mutex::new(false)),
        };

        restore_after_failed_activation(Some(&previous), &roots, &service).await?;
        discard_failed_release(&failed, &roots.releases)?;

        assert_eq!(
            active_halves(&roots)?,
            ("previous".to_string(), "previous".to_string())
        );
        assert!(!failed.exists(), "the failed release leaves nothing behind");
        assert_eq!(*operations.lock().unwrap(), ["stop", "start", "ready"]);
        Ok(())
    }

    /// An archive built before releases owned their supervisor is a bundle at its
    /// own root. Installing it would leave the unit executing that bundle with
    /// whatever supervisor the host happened to have, so the installer refuses it by
    /// name and nothing is activated.
    #[tokio::test]
    async fn an_archive_predating_supervisor_owning_releases_is_refused_at_the_installer()
    -> Result<()> {
        let temp = tempfile::tempdir()?;
        let roots = roots(&temp);
        let archive = temp.path().join("legacy.build.phoxal");
        let encoder = flate2::write::GzEncoder::new(
            std::fs::File::create(&archive)?,
            flate2::Compression::fast(),
        );
        let mut builder = tar::Builder::new(encoder);
        for (name, bytes) in [
            ("manifest.json", b"{}".as_slice()),
            ("bin/brain", b"\x7fELF".as_slice()),
        ] {
            let mut header = tar::Header::new_gnu();
            header.set_size(bytes.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder.append_data(&mut header, name, bytes)?;
        }
        builder.into_inner()?.finish()?;
        // The archive is well-formed and properly attested; what makes it
        // uninstallable is its shape, and that is what this proves.
        std::fs::write(
            phoxal_cli_project::digest_sidecar(&archive),
            format!(
                "{}\n",
                hex::encode(<sha2::Sha256 as sha2::Digest>::digest(std::fs::read(
                    &archive
                )?))
            ),
        )?;

        let operations = std::sync::Arc::new(Mutex::new(Vec::new()));
        let error = install_archive(
            &archive,
            &roots,
            &ServiceControl::Fake {
                operations: operations.clone(),
                fail_readiness_once: std::sync::Arc::new(Mutex::new(false)),
            },
            true,
        )
        .await
        .expect_err("a bundle-shaped archive is not a deployment release");
        let error = format!("{error:#}");

        assert!(
            error.contains("predates supervisor-owning releases"),
            "{error}"
        );
        assert!(error.contains("rebuild it with this CLI"), "{error}");
        assert!(
            operations.lock().unwrap().is_empty(),
            "the service is never touched for an archive that cannot install"
        );
        assert!(active_release(&roots.active)?.is_none());
        assert!(
            std::fs::read_dir(&roots.releases)?.next().is_none(),
            "nothing is left behind in the releases root"
        );
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
