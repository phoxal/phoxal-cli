//! Installing, removing, and inspecting the managed `phoxal.service` unit.

use std::collections::BTreeMap;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};

use crate::cli::AppContext;

struct ServiceInstall;
struct ServiceUninstall;
struct ServiceStatus;

const UNIT_PATH: &str = phoxal_cli_project::SYSTEMD_UNIT_PATH;
const UNIT_MARKER: &str = "# Managed by phoxal";
const LEGACY_INSTALL_ROOT: &str = "/opt/phoxal";

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
Group=phoxal-engineering
SupplementaryGroups=phoxal
WorkingDirectory={active}
ExecStart=/usr/local/bin/phoxal start {active}
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
        active = phoxal_cli_project::ACTIVE_RUNTIME_ROOT,
        state = phoxal_cli_project::INSTALLED_STATE_ROOT,
        volatile = phoxal_cli_project::INSTALLED_VOLATILE_ROOT,
    )
}

impl ServiceInstall {
    async fn run(&self, app: &AppContext) -> Result<()> {
        require_root()?;
        require_systemd()?;
        let legacy = sweep_legacy_units(
            Path::new(phoxal_cli_project::SYSTEMD_UNIT_ROOT),
            Path::new(LEGACY_INSTALL_ROOT),
            &HostSystemctl,
        )?;
        for path in &legacy.skipped_foreign {
            app.ui.warn(format!(
                "left same-named foreign systemd entry untouched: {}",
                path.display()
            ));
        }
        if !legacy.removed_units.is_empty() {
            app.ui.info(format!(
                "removed legacy {} systemd wiring; preserved {}",
                legacy.removed_units.join(", "),
                LEGACY_INSTALL_ROOT
            ));
        }
        ensure_group("phoxal", true)?;
        ensure_group("phoxal-engineering", false)?;
        ensure_service_user()?;
        ensure_runtime_paths()?;
        write_managed_unit(Path::new(UNIT_PATH))?;
        run_status("systemctl", &["daemon-reload"])?;
        run_status("systemctl", &["enable", phoxal_cli_project::SYSTEMD_UNIT])?;
        app.ui
            .info("installed the single phoxal.service; install a build.phoxal before starting it");
        Ok(())
    }
}

impl ServiceUninstall {
    async fn run(&self, app: &AppContext) -> Result<()> {
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
        app.ui.info(
            "removed phoxal.service; releases, state, users, and hardware-group membership were preserved",
        );
        Ok(())
    }
}

impl ServiceStatus {
    async fn run(&self, _app: &AppContext) -> Result<()> {
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

fn ensure_group(name: &str, system: bool) -> Result<()> {
    if Command::new("getent")
        .args(["group", name])
        .status()
        .is_ok_and(|status| status.success())
    {
        return Ok(());
    }
    let mut args = Vec::new();
    if system {
        args.push("--system");
    }
    args.push(name);
    run_status("groupadd", &args)
}

fn ensure_service_user() -> Result<()> {
    if Command::new("id")
        .args(["-u", "phoxal"])
        .status()
        .is_ok_and(|status| status.success())
    {
        return run_status(
            "usermod",
            &[
                "--gid",
                "phoxal",
                "--append",
                "--groups",
                "phoxal-engineering",
                "phoxal",
            ],
        );
    }
    run_status(
        "useradd",
        &[
            "--system",
            "--gid",
            "phoxal",
            "--groups",
            "phoxal-engineering",
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
    use std::os::unix::fs::PermissionsExt;
    for path in [
        phoxal_cli_project::RELEASES_ROOT,
        phoxal_cli_project::INSTALLED_STATE_ROOT,
        phoxal_cli_project::INSTALLED_VOLATILE_ROOT,
    ] {
        std::fs::create_dir_all(path)?;
    }
    std::fs::set_permissions(
        phoxal_cli_project::RELEASES_ROOT,
        std::fs::Permissions::from_mode(0o755),
    )?;
    run_status("chown", &["root:root", phoxal_cli_project::RELEASES_ROOT])?;
    for path in [
        phoxal_cli_project::INSTALLED_STATE_ROOT,
        phoxal_cli_project::INSTALLED_VOLATILE_ROOT,
    ] {
        run_status("chown", &["phoxal:phoxal-engineering", path])?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o2775))?;
    }
    let lock = Path::new(phoxal_cli_project::INSTALLED_STATE_ROOT).join("project.lock");
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock)?
        .sync_all()?;
    run_status(
        "chown",
        &["phoxal:phoxal-engineering", lock.to_string_lossy().as_ref()],
    )?;
    std::fs::set_permissions(lock, std::fs::Permissions::from_mode(0o660))?;
    Ok(())
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

trait Systemctl {
    fn disable_now(&self, unit: &str) -> Result<()>;
}

struct HostSystemctl;

impl Systemctl for HostSystemctl {
    fn disable_now(&self, unit: &str) -> Result<()> {
        run_status("systemctl", &["disable", "--now", unit])
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
struct LegacySweep {
    removed_units: Vec<String>,
    skipped_foreign: Vec<PathBuf>,
}

fn sweep_legacy_units(
    systemd_root: &Path,
    legacy_root: &Path,
    systemctl: &impl Systemctl,
) -> Result<LegacySweep> {
    let mut links_by_unit = BTreeMap::<String, Vec<PathBuf>>::new();
    let mut skipped_foreign = Vec::new();
    let canonical_legacy_root = match legacy_root.canonicalize() {
        Ok(path) => Some(path),
        Err(error) if error.kind() == ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    for directory in [
        systemd_root.to_path_buf(),
        systemd_root.join("phoxal.target.wants"),
        systemd_root.join("multi-user.target.wants"),
    ] {
        let entries = match std::fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        for entry in entries {
            let entry = entry?;
            let Some(unit) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if !is_legacy_unit_name(&unit) {
                continue;
            }
            let path = entry.path();
            if is_confirmed_legacy_link(&path, canonical_legacy_root.as_deref()) {
                links_by_unit.entry(unit).or_default().push(path);
            } else {
                skipped_foreign.push(path);
            }
        }
    }

    let mut units = links_by_unit.keys().cloned().collect::<Vec<_>>();
    units.sort_by(|left, right| {
        legacy_unit_rank(left)
            .cmp(&legacy_unit_rank(right))
            .then_with(|| left.cmp(right))
    });
    for unit in &units {
        systemctl.disable_now(unit)?;
        for path in &links_by_unit[unit] {
            match std::fs::symlink_metadata(path) {
                Ok(_) if is_confirmed_legacy_link(path, canonical_legacy_root.as_deref()) => {
                    std::fs::remove_file(path)?;
                    sync_parent(path)?;
                }
                Ok(_) => anyhow::bail!(
                    "refusing to remove changed or foreign systemd entry {}",
                    path.display()
                ),
                Err(error) if error.kind() == ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
    }
    let legacy_wants = systemd_root.join("phoxal.target.wants");
    match std::fs::remove_dir(&legacy_wants) {
        Ok(()) => sync_parent(&legacy_wants)?,
        Err(error)
            if matches!(
                error.kind(),
                ErrorKind::NotFound | ErrorKind::DirectoryNotEmpty
            ) => {}
        Err(error) => return Err(error.into()),
    }
    skipped_foreign.sort();
    Ok(LegacySweep {
        removed_units: units,
        skipped_foreign,
    })
}

fn is_legacy_unit_name(name: &str) -> bool {
    name == "phoxal.target"
        || name == "phoxal-router.service"
        || (name
            .strip_prefix("phoxal-participant-")
            .and_then(|suffix| suffix.strip_suffix(".service"))
            .is_some_and(|participant| !participant.is_empty()))
}

fn legacy_unit_rank(name: &str) -> u8 {
    match name {
        "phoxal.target" => 0,
        "phoxal-router.service" => 1,
        _ => 2,
    }
}

fn is_confirmed_legacy_link(path: &Path, canonical_legacy_root: Option<&Path>) -> bool {
    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        return false;
    };
    if !metadata.file_type().is_symlink() {
        return false;
    }
    let Some(legacy_root) = canonical_legacy_root else {
        return false;
    };
    path.canonicalize()
        .is_ok_and(|target| target.starts_with(legacy_root))
}

pub(crate) async fn install(app: &AppContext) -> Result<()> {
    ServiceInstall.run(app).await
}

pub(crate) async fn uninstall(app: &AppContext) -> Result<()> {
    ServiceUninstall.run(app).await
}

pub(crate) async fn status(app: &AppContext) -> Result<()> {
    ServiceStatus.run(app).await
}

pub(crate) async fn service_install_command(app: &AppContext) -> Result<()> {
    install(app).await
}

pub(crate) async fn service_uninstall_command(app: &AppContext) -> Result<()> {
    uninstall(app).await
}

pub(crate) async fn service_status_command(app: &AppContext) -> Result<()> {
    status(app).await
}

#[cfg(test)]
mod unit_tests {
    use std::cell::RefCell;
    use std::os::unix::fs::symlink;

    use super::*;

    #[test]
    fn managed_service_renders_one_resident_runtime_authority() {
        let unit = unit_contents();
        assert_eq!(unit.matches("ExecStart=").count(), 1);
        assert!(unit.contains("ExecStart=/usr/local/bin/phoxal start /var/phoxal"));
        assert!(unit.contains("Type=notify"));
        assert!(unit.contains("NotifyAccess=main"));
        assert!(unit.contains("WatchdogSec=30s"));
        assert!(unit.contains("User=phoxal\nGroup=phoxal-engineering"));
        assert!(!unit.contains("StateDirectory="));
        assert!(!unit.contains("participant"));
    }

    #[derive(Default)]
    struct FakeSystemctl {
        disabled: RefCell<Vec<String>>,
    }

    impl Systemctl for FakeSystemctl {
        fn disable_now(&self, unit: &str) -> Result<()> {
            self.disabled.borrow_mut().push(unit.to_string());
            Ok(())
        }
    }

    #[test]
    fn legacy_sweep_removes_only_opt_phoxal_unit_links_in_safe_order() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let systemd = temp.path().join("etc/systemd/system");
        let legacy = temp.path().join("opt/phoxal");
        let foreign = temp.path().join("foreign");
        std::fs::create_dir_all(systemd.join("phoxal.target.wants"))?;
        std::fs::create_dir_all(systemd.join("multi-user.target.wants"))?;
        std::fs::create_dir_all(legacy.join("systemd"))?;
        std::fs::create_dir_all(&foreign)?;
        for unit in [
            "phoxal.target",
            "phoxal-router.service",
            "phoxal-participant-drive.service",
        ] {
            std::fs::write(legacy.join("systemd").join(unit), "[Unit]\n")?;
            symlink(legacy.join("systemd").join(unit), systemd.join(unit))?;
        }
        symlink(
            legacy.join("systemd/phoxal.target"),
            systemd.join("multi-user.target.wants/phoxal.target"),
        )?;
        symlink(
            legacy.join("systemd/phoxal-router.service"),
            systemd.join("phoxal.target.wants/phoxal-router.service"),
        )?;
        symlink(
            legacy.join("systemd/phoxal-participant-drive.service"),
            systemd.join("phoxal.target.wants/phoxal-participant-drive.service"),
        )?;
        std::fs::write(systemd.join("phoxal.service"), "# Managed by phoxal\n")?;
        std::fs::write(foreign.join("phoxal-participant-map.service"), "[Unit]\n")?;
        symlink(
            foreign.join("phoxal-participant-map.service"),
            systemd.join("phoxal-participant-map.service"),
        )?;
        std::fs::write(
            systemd.join("phoxal-participant-safety.service"),
            "[Unit]\n",
        )?;
        let systemctl = FakeSystemctl::default();

        let result = sweep_legacy_units(&systemd, &legacy, &systemctl)?;

        assert_eq!(
            result.removed_units,
            [
                "phoxal.target",
                "phoxal-router.service",
                "phoxal-participant-drive.service"
            ]
        );
        assert_eq!(
            *systemctl.disabled.borrow(),
            result.removed_units,
            "the target must stop before its router and participants"
        );
        assert_eq!(
            result.skipped_foreign,
            [
                systemd.join("phoxal-participant-map.service"),
                systemd.join("phoxal-participant-safety.service")
            ]
        );
        assert!(systemd.join("phoxal.service").is_file());
        assert!(legacy.join("systemd/phoxal.target").is_file());
        assert!(!systemd.join("phoxal.target").exists());
        assert!(!systemd.join("phoxal.target.wants").exists());
        Ok(())
    }

    #[test]
    fn legacy_unit_name_never_selects_the_resident_service() {
        assert!(is_legacy_unit_name("phoxal.target"));
        assert!(is_legacy_unit_name("phoxal-router.service"));
        assert!(is_legacy_unit_name("phoxal-participant-drive.service"));
        assert!(!is_legacy_unit_name("phoxal.service"));
        assert!(!is_legacy_unit_name("phoxal-participant-.service"));
        assert!(!is_legacy_unit_name("phoxal-participant-drive.timer"));
    }
}
