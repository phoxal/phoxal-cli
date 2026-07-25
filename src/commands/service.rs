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

impl Suite {
    pub async fn run(&self, app: &AppContext) -> Result<()> {
        let root = app.project.root().to_path_buf();
        let suite_source = app.suite_source.clone();
        let summary =
            tokio::task::spawn_blocking(move || service_suite_summary(&root, suite_source))
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
}

pub fn service_suite_summary(
    project_start: &Path,
    suite_source: Option<String>,
) -> Result<ServiceSuiteSummary> {
    let robot_path = discover_robot_yaml(project_start)
        .with_context(|| format!("failed to find robot.yaml from {}", project_start.display()))?;
    let project_root = robot_path
        .parent()
        .context("robot.yaml did not have a parent directory")?;
    // Keep `service suite` project-bound: malformed robot intent must fail
    // before presenting an inventory for that project.
    let _ = load_robot(&robot_path)?;
    let suite = crate::commands::load_suite_for_robot_from_source(suite_source, project_root)?
        .ok_or_else(|| anyhow::anyhow!("artifact suite unavailable"))?;
    Ok(ServiceSuiteSummary {
        entries: phoxal_cli_core::project::suite::artifacts_of_kind(
            &suite,
            phoxal_cli_core::project::suite::Kind::Service,
        )
        .into_iter()
        .map(|artifact| ServiceSuiteEntry {
            id: artifact.id.clone(),
            version: suite.version.clone(),
            participant_kind: "service",
        })
        .collect(),
    })
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use super::*;
    use phoxal_cli_core::project::suite::{
        fixture_contract_for_tests, fixture_service_entry_for_tests, fixture_suite_for_tests,
    };

    #[test]
    fn service_suite_summary_lists_official_services() -> Result<()> {
        let temp = tempfile::tempdir()?;
        fs::write(temp.path().join("robot.yaml"), minimal_robot_yaml())?;
        let suite = write_suite(temp.path())?;

        let summary = service_suite_summary(temp.path(), Some(suite))?;

        assert_eq!(summary.entries.len(), 1);
        let entry = summary
            .entries
            .iter()
            .find(|entry| entry.id == "phoxal/service-drive")
            .expect("drive is part of the platform model");
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

    fn write_suite(root: &Path) -> Result<String> {
        let suite = fixture_suite_for_tests(vec![fixture_service_entry_for_tests(
            "drive",
            "0.1.0",
            &crate::resolver::host_target_triple(),
            true,
            vec![fixture_contract_for_tests("v0.1::drive::Target", "publish")],
        )]);
        let path = root.join("suite.json");
        fs::write(&path, serde_json::to_string_pretty(&suite)?)?;
        Ok(path.display().to_string())
    }
}
