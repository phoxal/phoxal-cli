//! Installing, removing, and inspecting the managed `phoxal.service` unit.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};

use crate::cli::context::AppContext;

const UNIT_PATH: &str = phoxal_cli_project::SYSTEMD_UNIT_PATH;
const UNIT_MARKER: &str = "# Managed by phoxal";

/// The managed unit.
///
/// `ExecStart` is the daemon and its bundle root, and nothing else
///: `phoxald <INSTALLED_BUNDLE_ROOT>`. The interactive
/// client is never run as the daemon - it builds, and a durable systemd-owned
/// service must never acquire Cargo, registries, a toolchain, or a terminal.
/// `Type=notify` because the daemon owns READY and the watchdog itself.
fn unit_contents() -> String {
    format!(
        r#"# Managed by phoxal
[Unit]
Description=Phoxal robot runtime
After=network-online.target
Wants=network-online.target

[Service]
Type=notify
NotifyAccess=main
User=phoxal
Group=phoxal
WorkingDirectory={active}
ExecStart={daemon} {active}
Restart=on-failure
RestartSec=2s
WatchdogSec=30s
TimeoutStartSec=300s
TimeoutStopSec=300s
KillMode=control-group
UMask=0007
RuntimeDirectory=phoxal
RuntimeDirectoryMode=2775
ProtectSystem=strict
ProtectHome=true
ReadWritePaths={state} {volatile}

[Install]
WantedBy=multi-user.target
"#,
        daemon = phoxal_cli_project::INSTALLED_DAEMON_BINARY,
        active = phoxal_cli_project::ACTIVE_RUNTIME_ROOT,
        state = phoxal_cli_project::INSTALLED_STATE_ROOT,
        volatile = phoxal_cli_project::INSTALLED_VOLATILE_ROOT,
    )
}

pub(crate) async fn install(app: &AppContext) -> Result<()> {
    require_root()?;
    require_systemd()?;
    ensure_service_group()?;
    ensure_service_user()?;
    ensure_runtime_paths()?;
    write_managed_unit(Path::new(UNIT_PATH))?;
    run_status("systemctl", &["daemon-reload"])?;
    run_status("systemctl", &["enable", phoxal_cli_project::SYSTEMD_UNIT])?;
    app.ui
        .info("installed the single phoxal.service; install a build.phoxal before starting it");
    Ok(())
}

pub(crate) async fn uninstall(app: &AppContext) -> Result<()> {
    require_root()?;
    require_systemd()?;
    let path = Path::new(UNIT_PATH);
    if path.exists() {
        let contents = std::fs::read_to_string(path)?;
        anyhow::ensure!(
            contents.starts_with(UNIT_MARKER),
            "refusing to remove foreign unit {}",
            path.display()
        );
        let _ = run_status(
            "systemctl",
            &["disable", "--now", phoxal_cli_project::SYSTEMD_UNIT],
        );
        std::fs::remove_file(path)?;
        sync_parent(path)?;
        run_status("systemctl", &["daemon-reload"])?;
    }
    app.ui
        .info("removed phoxal.service; releases, state, and the phoxal user were preserved");
    Ok(())
}

pub(crate) async fn status(_app: &AppContext) -> Result<()> {
    require_systemd()?;
    run_status(
        "systemctl",
        &[
            "status",
            "--no-pager",
            "--full",
            phoxal_cli_project::SYSTEMD_UNIT,
        ],
    )
}

fn require_root() -> Result<()> {
    anyhow::ensure!(
        unsafe { libc::geteuid() } == 0,
        "`phoxal service install` and `uninstall` require root"
    );
    Ok(())
}

fn require_systemd() -> Result<()> {
    anyhow::ensure!(
        Path::new(phoxal_cli_project::SYSTEMD_ACTIVE_ROOT).is_dir(),
        "systemd is not the active service manager on this host"
    );
    Ok(())
}

/// The unit runs as its own `phoxal` user, so the user's own group has to exist
/// before `useradd --gid` can name it.
fn ensure_service_group() -> Result<()> {
    if Command::new("getent")
        .args(["group", "phoxal"])
        .status()
        .is_ok_and(|status| status.success())
    {
        return Ok(());
    }
    run_status("groupadd", &["--system", "phoxal"])
}

fn ensure_service_user() -> Result<()> {
    if Command::new("id")
        .args(["-u", "phoxal"])
        .status()
        .is_ok_and(|status| status.success())
    {
        return run_status("usermod", &["--gid", "phoxal", "phoxal"]);
    }
    run_status(
        "useradd",
        &[
            "--system",
            "--gid",
            "phoxal",
            "--home-dir",
            phoxal_cli_project::INSTALL_ROOT,
            "--shell",
            "/usr/sbin/nologin",
            "phoxal",
        ],
    )
}

fn ensure_runtime_paths() -> Result<()> {
    use std::fs::OpenOptions;
    for path in [
        phoxal_cli_project::RELEASES_ROOT,
        phoxal_cli_project::INSTALLED_STATE_ROOT,
        phoxal_cli_project::INSTALLED_VOLATILE_ROOT,
    ] {
        std::fs::create_dir_all(path)?;
    }
    // The unit runs as `phoxal`, so the two roots it writes have to belong to it.
    for path in [
        phoxal_cli_project::INSTALLED_STATE_ROOT,
        phoxal_cli_project::INSTALLED_VOLATILE_ROOT,
    ] {
        run_status("chown", &["phoxal:phoxal", path])?;
    }
    let lock = Path::new(phoxal_cli_project::INSTALLED_STATE_ROOT).join("project.lock");
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock)?
        .sync_all()?;
    run_status("chown", &["phoxal:phoxal", lock.to_string_lossy().as_ref()])
}

fn write_managed_unit(path: &Path) -> Result<()> {
    if path.exists() {
        let current = std::fs::read_to_string(path)?;
        anyhow::ensure!(
            current.starts_with(UNIT_MARKER),
            "refusing to overwrite foreign unit {}",
            path.display()
        );
        if current == unit_contents() {
            return Ok(());
        }
    }
    let candidate = PathBuf::from(format!(
        "{}.candidate-{}",
        path.display(),
        std::process::id()
    ));
    std::fs::write(&candidate, unit_contents())?;
    std::fs::File::open(&candidate)?.sync_all()?;
    std::fs::rename(&candidate, path)?;
    sync_parent(path)
}

fn sync_parent(path: &Path) -> Result<()> {
    std::fs::File::open(path.parent().context("path has no parent")?)?.sync_all()?;
    Ok(())
}

fn run_status(program: &str, args: &[&str]) -> Result<()> {
    let status = Command::new(program).args(args).status()?;
    anyhow::ensure!(
        status.success(),
        "{} {} failed with {status}",
        program,
        args.join(" ")
    );
    Ok(())
}

#[cfg(test)]
mod unit_tests {
    use super::*;

    /// The unit runs the daemon on the installed bundle root and nothing else.
    /// The interactive client is never the daemon: it builds, and no `phoxal`
    /// invocation may appear in `ExecStart`.
    #[test]
    fn managed_service_executes_the_daemon_on_the_installed_bundle_root() {
        let unit = unit_contents();
        assert_eq!(unit.matches("ExecStart=").count(), 1);
        assert!(unit.contains("ExecStart=/usr/local/bin/phoxald /var/phoxal"));
        let exec_start = unit
            .lines()
            .find(|line| line.starts_with("ExecStart="))
            .expect("the unit has an ExecStart");
        assert!(
            !exec_start.contains("/phoxal "),
            "the interactive client must never be run as the daemon: {exec_start}"
        );
        for absent in [" start ", " run ", "--drivers", "--driver"] {
            assert!(
                !exec_start.contains(absent),
                "the daemon takes a bundle root and nothing else: {exec_start}"
            );
        }
        assert!(unit.contains("Type=notify"));
        assert!(unit.contains("NotifyAccess=main"));
        assert!(unit.contains("WatchdogSec=30s"));
        assert!(unit.contains("User=phoxal\nGroup=phoxal"));
        assert!(!unit.contains("StateDirectory="));
        assert!(!unit.contains("participant"));
    }
}
