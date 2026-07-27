use std::collections::BTreeMap;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use clap::{Args, Subcommand};
use serde::Serialize;

use crate::AppContext;
use phoxal_cli_core::project::resolver::{discover_robot_yaml, load_robot};

#[derive(Debug, Args)]
pub struct Service {
    #[command(subcommand)]
    pub command: ServiceSubcommand,
}

#[derive(Debug, Subcommand)]
pub enum ServiceSubcommand {
    #[command(about = "Install the one systemd phoxal.service and generic runtime paths.")]
    Install(ServiceInstall),
    #[command(about = "Disable and remove phoxal.service without deleting releases.")]
    Uninstall(ServiceUninstall),
    #[command(about = "Show the live systemd state for phoxal.service.")]
    Status(ServiceStatus),
    #[command(about = "Print official services from the configured artifact suite.")]
    Suite(Suite),
}

#[derive(Debug, Args)]
pub struct ServiceInstall {}

#[derive(Debug, Args)]
pub struct ServiceUninstall {}

#[derive(Debug, Args)]
pub struct ServiceStatus {}

#[derive(Debug, Args)]
pub struct Suite {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ServiceSuiteSummary {
    pub entries: Vec<ServiceSuiteEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ServiceSuiteEntry {
    pub id: String,
    pub version: String,
    pub participant_kind: &'static str,
}

impl Service {
    pub async fn run(&self, app: &AppContext) -> Result<()> {
        match &self.command {
            ServiceSubcommand::Install(command) => command.run(app).await,
            ServiceSubcommand::Uninstall(command) => command.run(app).await,
            ServiceSubcommand::Status(command) => command.run(app).await,
            ServiceSubcommand::Suite(command) => command.run(app).await,
        }
    }
}

const UNIT_PATH: &str = "/etc/systemd/system/phoxal.service";
const UNIT_MARKER: &str = "# Managed by phoxal";
const SYSTEMD_UNIT_ROOT: &str = "/etc/systemd/system";
const LEGACY_INSTALL_ROOT: &str = "/opt/phoxal";

fn unit_contents() -> &'static str {
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
WorkingDirectory=/var/phoxal
ExecStart=/usr/local/bin/phoxal start /var/phoxal
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
ReadWritePaths=/var/lib/phoxal/state /run/phoxal

[Install]
WantedBy=multi-user.target
"#
}

impl ServiceInstall {
    async fn run(&self, app: &AppContext) -> Result<()> {
        require_root()?;
        require_systemd()?;
        let legacy = sweep_legacy_units(
            Path::new(SYSTEMD_UNIT_ROOT),
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
        run_status("systemctl", &["enable", "phoxal.service"])?;
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
            let _ = run_status("systemctl", &["disable", "--now", "phoxal.service"]);
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
            &["status", "--no-pager", "--full", "phoxal.service"],
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
        Path::new("/run/systemd/system").is_dir(),
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
            crate::runtime_paths::INSTALL_ROOT,
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
        crate::runtime_paths::RELEASES_ROOT,
        crate::runtime_paths::INSTALLED_STATE_ROOT,
        crate::runtime_paths::INSTALLED_VOLATILE_ROOT,
    ] {
        std::fs::create_dir_all(path)?;
    }
    std::fs::set_permissions(
        crate::runtime_paths::RELEASES_ROOT,
        std::fs::Permissions::from_mode(0o755),
    )?;
    run_status("chown", &["root:root", crate::runtime_paths::RELEASES_ROOT])?;
    for path in [
        crate::runtime_paths::INSTALLED_STATE_ROOT,
        crate::runtime_paths::INSTALLED_VOLATILE_ROOT,
    ] {
        run_status("chown", &["phoxal:phoxal-engineering", path])?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o2775))?;
    }
    let lock = Path::new(crate::runtime_paths::INSTALLED_STATE_ROOT).join("project.lock");
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

impl Suite {
    pub async fn run(&self, app: &AppContext) -> Result<()> {
        let root = app.project.root().to_path_buf();
        let offline = app.offline;
        let summary = tokio::task::spawn_blocking(move || service_suite_summary(&root, offline))
            .await
            .context("service suite worker failed")??;
        for entry in &summary.entries {
            println!(
                "{} -> version {} ({})",
                entry.id, entry.version, entry.participant_kind
            );
        }
        Ok(())
    }
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

pub fn service_suite_summary(project_start: &Path, offline: bool) -> Result<ServiceSuiteSummary> {
    let robot_path = discover_robot_yaml(project_start)
        .with_context(|| format!("failed to find robot.yaml from {}", project_start.display()))?;
    let project_root = robot_path
        .parent()
        .context("robot.yaml did not have a parent directory")?;
    // Keep `service suite` project-bound: malformed robot intent must fail
    // before presenting an inventory for that project.
    let _ = load_robot(&robot_path)?;
    let train =
        phoxal_cli_core::project::train::resolve_locked_train(project_root, offline)?.version;
    Ok(ServiceSuiteSummary {
        entries: phoxal_cli_core::project::catalog::NATIVE
            .iter()
            .filter(|official| {
                official.kind == phoxal_cli_core::project::catalog::ArtifactKind::Service
            })
            .map(|official| ServiceSuiteEntry {
                id: official.package.to_string(),
                version: train.clone(),
                participant_kind: "service",
            })
            .collect(),
    })
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    #[test]
    fn service_suite_summary_lists_official_services() -> Result<()> {
        let temp = tempfile::tempdir()?;
        fs::write(temp.path().join("robot.yaml"), minimal_robot_yaml())?;
        write_locked_project(temp.path())?;

        let summary = service_suite_summary(temp.path(), false)?;

        let entry = summary
            .entries
            .iter()
            .find(|entry| entry.id == "phoxal/service-drive")
            .expect("drive is part of the catalog");
        assert_eq!(entry.id, "phoxal/service-drive");
        assert_eq!(entry.version, "0.1.0");
        assert_eq!(entry.participant_kind, "service");

        Ok(())
    }

    fn minimal_robot_yaml() -> &'static str {
        r#"schema: robot/v0
robot:
  id: testbot
  namespace: test
  motion_limits:
    max_linear_speed_mps: 0.6
    max_angular_speed_radps: 2.0
  kinematic:
    kind: differential
    left_actuators: [left_drive.motor]
    right_actuators: [right_drive.motor]
    left_encoders: [left_drive.encoder]
    right_encoders: [right_drive.encoder]
    wheel_radius_m: 0.1
    wheel_base_m: 0.5
  components: {}
"#
    }

    /// A minimal locked Cargo project resolving to train `0.1.0` via a local
    /// path dependency, so `resolve_locked_train` needs no network.
    fn write_locked_project(root: &Path) -> Result<()> {
        fs::create_dir_all(root.join("src"))?;
        fs::create_dir_all(root.join("train/phoxal/src"))?;
        fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"robot\"\nversion = \"0.1.0\"\nedition = \"2024\"\npublish = false\n\n[dependencies]\nphoxal = { path = \"train/phoxal\" }\n",
        )?;
        fs::write(root.join("src/lib.rs"), "")?;
        fs::write(
            root.join("train/phoxal/Cargo.toml"),
            "[package]\nname = \"phoxal\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        )?;
        fs::write(root.join("train/phoxal/src/lib.rs"), "")?;
        fs::write(
            root.join("Cargo.lock"),
            "version = 4\n\n[[package]]\nname = \"phoxal\"\nversion = \"0.1.0\"\n\n[[package]]\nname = \"robot\"\nversion = \"0.1.0\"\ndependencies = [\"phoxal\"]\n",
        )?;
        Ok(())
    }
}
