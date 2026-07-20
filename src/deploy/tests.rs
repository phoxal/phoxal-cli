//! Tests for this module.

use super::cross_build::classify_zigbuild_failure;
use super::execute::{
    SUDO_STDIN_PROMPT, deploy_with_transport_with_sudo, ensure_sudo_will_succeed,
};
use super::metadata::locate_cached_component_assets_dir;
use super::official::official_runtime_plan;
use super::*;
use anyhow::{Context, Result, bail};
use phoxal::model::robot::v0::Robot;
use phoxal_cli_core::deploy::target_from_selector;
use phoxal_cli_core::project::launch_plan::SITE_INFRASTRUCTURE_ROUTER;
use phoxal_cli_core::project::resolver::{
    ResolvedComponentSource, ResolvedPlatformRuntime, ResolvedRobot, ResolvedTool,
};
use phoxal_cli_core::project::tooling::make_executable;
use std::collections::{BTreeSet, VecDeque};
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use clap::Parser;
use phoxal_cli_core::project::resolver::ResolvedComponent;
use phoxal_cli_test_support::write_basic_project;

mod phoxal_cli_test_support {
    use super::*;
    use phoxal_cli_core::project::catalog::{
        SelectionChannel as CatalogChannel, fixture_tool_entry_for_tests,
    };

    pub fn write_basic_project(root: &Path) -> Result<()> {
        write_fixture_catalog(root)?;
        fs::write(root.join("robot.yaml"), basic_robot_yaml())?;
        fs::write(root.join("robot.dev.yaml"), basic_robot_dev_overlay_yaml())?;
        write_robot_structure(root)?;
        write_service_crate(root, "navtask", "service", "navtask")?;
        write_component_metadata(root, "ddsm115")?;
        Ok(())
    }

    pub fn write_driver_project(root: &Path) -> Result<()> {
        write_fixture_catalog(root)?;
        fs::write(root.join("robot.yaml"), driver_robot_yaml())?;
        fs::write(root.join("robot.dev.yaml"), driver_robot_dev_overlay_yaml())?;
        write_robot_structure(root)?;
        write_service_crate(root, "navtask", "service", "navtask")?;
        write_driver_crate(root, "ddsm115", "driver-ddsm115")?;
        write_component_metadata(root, "ddsm115")?;
        Ok(())
    }

    pub fn write_bench_camera_project(root: &Path) -> Result<()> {
        write_fixture_catalog(root)?;
        fs::write(root.join("robot.yaml"), bench_camera_robot_yaml())?;
        fs::write(
            root.join("robot.dev.yaml"),
            bench_camera_robot_dev_overlay_yaml(),
        )?;
        write_robot_structure(root)?;
        write_component_metadata(root, "bench_camera")?;
        let component_dir = root.join("components").join("bench_camera");
        fs::create_dir_all(component_dir.join("src"))?;
        fs::create_dir_all(component_dir.join("target/debug"))?;
        fs::write(
            component_dir.join("Cargo.toml"),
            "[package]\nname = \"bench-camera\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        )?;
        fs::write(component_dir.join("src/main.rs"), "fn main() {}\n")?;
        fs::write(component_dir.join("target/debug/ignored"), "ignored\n")?;
        Ok(())
    }

    pub fn write_catalog_only_project(root: &Path) -> Result<()> {
        write_fixture_catalog(root)?;
        fs::write(root.join("robot.yaml"), catalog_only_robot_yaml())?;
        write_robot_structure(root)?;
        Ok(())
    }

    pub fn write_native_dep_project(root: &Path) -> Result<()> {
        write_fixture_catalog(root)?;
        fs::write(root.join("robot.yaml"), basic_robot_yaml())?;
        fs::write(root.join("robot.dev.yaml"), basic_robot_dev_overlay_yaml())?;
        let dir = root.join("runtimes/navtask");
        fs::create_dir_all(dir.join("src"))?;
        fs::write(
            dir.join("Cargo.toml"),
            "[package]\nname = \"navtask\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\nopencv = \"0.1\"\n",
        )?;
        fs::write(dir.join("src/main.rs"), service_main("service", "navtask"))?;
        Ok(())
    }

    fn write_fixture_catalog(root: &Path) -> Result<()> {
        let catalog = phoxal_cli_core::project::catalog::fixture_catalog_for_tests(vec![
            // A decoy extra catalog entry proving multi-entry catalogs
            // resolve fine; kept off the `stable` channel every fixture
            // robot below targets, so it never becomes a real deploy
            // participant (the lean manifest schema has no separate
            // "declared target" concept to keep it inert by target alone
            // - see `resolver::select_latest_artifact_entries`).
            phoxal_cli_core::project::catalog::fixture_service_entry_for_tests(
                "fixture_only",
                "0.1.0",
                phoxal_cli_core::project::catalog::SelectionChannel::Nightly,
                "test-only-target",
                false,
                vec![
                    phoxal_cli_core::project::catalog::fixture_contract_for_tests(
                        "v1::fixture::Only",
                        "publish",
                    ),
                ],
            ),
            fixture_tool_entry_for_tests(
                "router",
                "0.1.0",
                CatalogChannel::Stable,
                "aarch64-unknown-linux-gnu",
                true,
                Vec::new(),
            ),
            fixture_tool_entry_for_tests(
                "router",
                "0.1.0",
                CatalogChannel::Stable,
                &crate::resolver::host_target_triple(),
                true,
                Vec::new(),
            ),
        ]);
        fs::write(
            root.join("catalog.json"),
            serde_json::to_string_pretty(&catalog)?,
        )?;
        Ok(())
    }

    fn write_service_crate(root: &Path, name: &str, kind: &str, artifact_id: &str) -> Result<()> {
        let dir = root.join("runtimes").join(name);
        fs::create_dir_all(dir.join("src"))?;
        fs::write(
            dir.join("Cargo.toml"),
            format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n"),
        )?;
        fs::write(dir.join("src/main.rs"), service_main(kind, artifact_id))?;
        Ok(())
    }

    fn write_driver_crate(root: &Path, name: &str, package: &str) -> Result<()> {
        let dir = root.join("components").join(name);
        fs::create_dir_all(dir.join("src"))?;
        fs::write(
            dir.join("Cargo.toml"),
            format!("[package]\nname = \"{package}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n"),
        )?;
        fs::write(dir.join("src/main.rs"), service_main("driver", name))?;
        Ok(())
    }

    fn write_robot_structure(root: &Path) -> Result<()> {
        fs::write(root.join("structure.urdf"), robot_structure_urdf())?;
        Ok(())
    }

    fn write_component_metadata(root: &Path, name: &str) -> Result<()> {
        let dir = root.join("components").join(name);
        fs::create_dir_all(&dir)?;
        fs::write(dir.join("component.yaml"), component_yaml())?;
        fs::write(dir.join("structure.urdf"), component_structure_urdf(name))?;
        Ok(())
    }

    fn service_main(kind: &str, artifact_id: &str) -> String {
        format!(
            "fn main() {{\n    if std::env::args().nth(1).as_deref() == Some(\"emit-apis\") {{\n        println!(\"{{}}\", r#\"{{\"artifact\":{{\"kind\":\"{kind}\",\"id\":\"{artifact_id}\"}},\"participant_class\":\"checked\",\"api_version\":\"source\",\"required_contracts\":[]}}\"#);\n    }}\n}}\n"
        )
    }

    fn basic_robot_yaml() -> &'static str {
        r#"schema: robot/v0
robot:
  id: testbot
  namespace: dev
  motion_limits:
    max_linear_speed_mps: 0.6
    max_angular_speed_radps: 2.0
  structure: structure.urdf
  kinematic:
    kind: differential
    left_actuators: [left_drive.motor]
    right_actuators: [right_drive.motor]
    left_encoders: [left_drive.encoder]
    right_encoders: [right_drive.encoder]
    wheel_radius_m: 0.1
    wheel_base_m: 0.5
  components:
    left_drive:
      component: ddsm115
      mount_link: left_wheel
    right_drive:
      component: ddsm115
      mount_link: right_wheel
artifacts:
  channel: stable
  catalog: catalog.json
services:
  navtask:
    path: runtimes/navtask
"#
    }

    /// Path pins are dev-overlay-only; every fixture project pairs its base
    /// `robot.yaml` with this `robot.dev.yaml` overlay (loaded via
    /// `--env dev`, see `dry_options`/`live_options`) so local component
    /// asset/driver directories resolve without a real catalog/network.
    fn basic_robot_dev_overlay_yaml() -> &'static str {
        r#"artifacts:
  pins:
    phoxal/component-ddsm115:
      path: components/ddsm115
"#
    }

    fn bench_camera_robot_yaml() -> &'static str {
        r#"schema: robot/v0
robot:
  id: benchbot
  namespace: dev
  motion_limits:
    max_linear_speed_mps: 0.6
    max_angular_speed_radps: 2.0
  structure: structure.urdf
  kinematic:
    kind: omnidirectional
    actuators: [front_camera.motor]
    encoders: [front_camera.encoder]
  components:
    front_camera:
      component: bench_camera
      mount_link: camera_mount
artifacts:
  channel: stable
  catalog: catalog.json
"#
    }

    fn bench_camera_robot_dev_overlay_yaml() -> &'static str {
        r#"artifacts:
  pins:
    phoxal/component-bench_camera:
      path: components/bench_camera
"#
    }

    fn catalog_only_robot_yaml() -> &'static str {
        r#"schema: robot/v0
robot:
  id: catalogbot
  namespace: dev
  motion_limits:
    max_linear_speed_mps: 0.6
    max_angular_speed_radps: 2.0
  structure: structure.urdf
  kinematic:
    kind: omnidirectional
    actuators: [catalog_drive.motor]
    encoders: [catalog_drive.encoder]
  components:
    catalog_drive:
      component: catalog_motor
      mount_link: left_wheel
artifacts:
  channel: stable
  catalog: catalog.json
  pins:
    phoxal/component-catalog_motor:
      git: /definitely/not/a/component-assets-repo
      rev: main
"#
    }

    fn driver_robot_yaml() -> &'static str {
        r#"schema: robot/v0
robot:
  id: testbot
  namespace: dev
  motion_limits:
    max_linear_speed_mps: 0.6
    max_angular_speed_radps: 2.0
  structure: structure.urdf
  kinematic:
    kind: differential
    left_actuators: [left_drive.motor]
    right_actuators: [right_drive.motor]
    left_encoders: [left_drive.encoder]
    right_encoders: [right_drive.encoder]
    wheel_radius_m: 0.1
    wheel_base_m: 0.5
  components:
    left_drive:
      component: ddsm115
      mount_link: left_wheel
      driver:
        connection: { type: serial, port: /dev/ttyUSB0, baud: 115200 }
    right_drive:
      component: ddsm115
      mount_link: right_wheel
      driver:
        connection: { type: i2c, bus: 1, address: 16 }
artifacts:
  channel: stable
  catalog: catalog.json
services:
  navtask:
    path: runtimes/navtask
"#
    }

    fn driver_robot_dev_overlay_yaml() -> &'static str {
        r#"artifacts:
  pins:
    phoxal/component-ddsm115:
      path: components/ddsm115
"#
    }

    fn robot_structure_urdf() -> &'static str {
        r#"<robot name="testbot">
  <link name="base_footprint" />
  <link name="base_link" />
  <link name="left_wheel" />
  <link name="right_wheel" />
  <link name="camera_mount" />
  <joint name="root" type="fixed">
    <parent link="base_footprint" />
    <child link="base_link" />
  </joint>
  <joint name="left_mount" type="fixed">
    <parent link="base_link" />
    <child link="left_wheel" />
  </joint>
  <joint name="right_mount" type="fixed">
    <parent link="base_link" />
    <child link="right_wheel" />
  </joint>
  <joint name="camera_mount_joint" type="fixed">
    <parent link="base_link" />
    <child link="camera_mount" />
  </joint>
</robot>
"#
    }

    fn component_yaml() -> &'static str {
        r#"schema: component/v0
structure: structure.urdf
capabilities:
  motor:
    kind: motor
    target: { kind: joint, id: wheel_joint }
    command: velocity
    gear_ratio: 1.0
  encoder:
    kind: encoder
    target: { kind: joint, id: wheel_joint }
    publish_rate_hz: 50.0
    gear_ratio: 1.0
  rgb:
    kind: camera
    target: { kind: link, id: camera_link }
    mode: rgb
    publish_rate_hz: 30.0
    width_px: 640
    height_px: 480
"#
    }

    fn component_structure_urdf(name: &str) -> String {
        format!(
            r#"<robot name="{name}">
  <link name="camera_link" />
  <link name="wheel_link" />
  <joint name="wheel_joint" type="continuous">
    <parent link="camera_link" />
    <child link="wheel_link" />
  </joint>
</robot>
"#
        )
    }
}

#[derive(Debug)]
struct FakeTransport {
    probe: RemoteProbe,
    installed_units: Vec<String>,
    health: HealthReport,
    bootstrapped: bool,
    bootstrap_fragment_seen: Option<String>,
    bootstrap_remote_user_seen: Option<String>,
    validation_results: VecDeque<bool>,
    validation_password_stdin: Vec<Vec<u8>>,
    bootstrap_sudo_command_seen: Option<Vec<String>>,
    bootstrap_password_stdin: Vec<Vec<u8>>,
    synced: bool,
    stale_removed: Vec<String>,
    restarted: bool,
    github_reachable: bool,
    downloaded: Vec<DownloadArtifact>,
    activated: Option<String>,
    rolled_back: bool,
    finalized: bool,
    fallback_prepared: bool,
    download_fails: bool,
    active_generation: Option<String>,
    previous_generation: Option<String>,
}

impl FakeTransport {
    fn healthy() -> Self {
        Self {
            probe: RemoteProbe {
                arch: "aarch64".to_string(),
                bootstrap_required: true,
                remote_user: "robot".to_string(),
                sudo_noninteractive: true,
                helper_grant: true,
                helper_stale: false,
            },
            installed_units: Vec::new(),
            health: HealthReport { units: Vec::new() },
            bootstrapped: false,
            bootstrap_fragment_seen: None,
            bootstrap_remote_user_seen: None,
            validation_results: VecDeque::from([true]),
            validation_password_stdin: Vec::new(),
            bootstrap_sudo_command_seen: None,
            bootstrap_password_stdin: Vec::new(),
            synced: false,
            stale_removed: Vec::new(),
            restarted: false,
            github_reachable: true,
            downloaded: Vec::new(),
            activated: None,
            rolled_back: false,
            finalized: false,
            fallback_prepared: false,
            download_fails: false,
            active_generation: Some("releases/previous-generation".to_string()),
            previous_generation: None,
        }
    }
}

impl DeployTransport for FakeTransport {
    fn probe(&mut self) -> Result<RemoteProbe> {
        Ok(self.probe.clone())
    }

    fn validate_sudo_password(&mut self, password: &SudoPassword) -> Result<bool> {
        let mut stdin = Vec::new();
        password.write_with_newline(&mut stdin)?;
        self.validation_password_stdin.push(stdin);
        Ok(self.validation_results.pop_front().unwrap_or(true))
    }

    fn bootstrap(
        &mut self,
        helper: &BootstrapScripts,
        sudo_password: Option<&SudoPassword>,
    ) -> Result<()> {
        self.bootstrapped = true;
        self.bootstrap_fragment_seen = Some(helper.sudoers_fragment.clone());
        self.bootstrap_remote_user_seen = Some(helper.remote_user.clone());
        self.bootstrap_sudo_command_seen = Some(args_to_strings(sudo_bootstrap_args(
            "/tmp/phoxal-bootstrap.TEST.sh",
        )));
        if let Some(password) = sudo_password {
            let mut stdin = Vec::new();
            password.write_with_newline(&mut stdin)?;
            self.bootstrap_password_stdin.push(stdin);
        }
        Ok(())
    }

    fn list_installed_units(&mut self) -> Result<Vec<String>> {
        Ok(self.installed_units.clone())
    }

    fn github_release_reachable(&mut self, _url: &str) -> Result<bool> {
        Ok(self.github_reachable)
    }

    fn prepare_host_transfer_fallback(
        &mut self,
        _payload: &mut RenderedPayload,
        _ui: &crate::Ui,
    ) -> Result<()> {
        self.fallback_prepared = true;
        Ok(())
    }

    fn sync_payload(&mut self, _payload: &RenderedPayload) -> Result<()> {
        self.synced = true;
        Ok(())
    }

    fn download_official_artifacts(
        &mut self,
        _generation: &str,
        artifacts: &[DownloadArtifact],
    ) -> Result<()> {
        if self.download_fails {
            bail!("simulated verification failure");
        }
        self.downloaded = artifacts.to_vec();
        Ok(())
    }

    fn install_units(&mut self, _payload: &RenderedPayload, stale_units: &[String]) -> Result<()> {
        self.stale_removed = stale_units.to_vec();
        Ok(())
    }

    fn activate_release(&mut self, generation: &str) -> Result<()> {
        self.previous_generation = self.active_generation.take();
        self.active_generation = Some(format!("releases/{generation}"));
        self.activated = Some(generation.to_string());
        Ok(())
    }

    fn rollback_release(&mut self) -> Result<()> {
        std::mem::swap(&mut self.active_generation, &mut self.previous_generation);
        self.rolled_back = true;
        Ok(())
    }

    fn finalize_units(&mut self, stale_units: &[String]) -> Result<()> {
        self.stale_removed = stale_units.to_vec();
        self.finalized = true;
        Ok(())
    }

    fn restart(&mut self) -> Result<()> {
        self.restarted = true;
        Ok(())
    }

    fn health_report(&mut self, units: &[String], _deadline: Duration) -> Result<HealthReport> {
        if self.health.units.is_empty() {
            Ok(HealthReport {
                units: units
                    .iter()
                    .map(|unit| HealthUnitReport {
                        unit: unit.clone(),
                        participant: participant_from_unit(unit),
                        ready: true,
                        active_state: "active".to_string(),
                        sub_state: "running".to_string(),
                        journal_excerpt: Vec::new(),
                    })
                    .collect(),
            })
        } else {
            Ok(self.health.clone())
        }
    }
}

#[derive(Debug)]
struct ScriptedSudoPasswordSource {
    env_password: Option<Vec<u8>>,
    prompt_passwords: VecDeque<Vec<u8>>,
    env_calls: usize,
    prompt_calls: usize,
    prompts_seen: Vec<String>,
}

impl ScriptedSudoPasswordSource {
    fn none() -> Self {
        Self {
            env_password: None,
            prompt_passwords: VecDeque::new(),
            env_calls: 0,
            prompt_calls: 0,
            prompts_seen: Vec::new(),
        }
    }

    fn with_env(password: &str) -> Self {
        let mut source = Self::none();
        source.env_password = Some(password.as_bytes().to_vec());
        source
    }

    fn with_prompts(passwords: &[&str]) -> Self {
        let mut source = Self::none();
        source.prompt_passwords = passwords
            .iter()
            .map(|password| password.as_bytes().to_vec())
            .collect();
        source
    }
}

impl SudoPasswordSource for ScriptedSudoPasswordSource {
    fn password_from_env(&mut self) -> Option<SudoPassword> {
        self.env_calls += 1;
        self.env_password.take().map(SudoPassword::new)
    }

    fn read_password(&mut self, prompt: &str) -> Result<SudoPassword> {
        self.prompt_calls += 1;
        self.prompts_seen.push(prompt.to_string());
        self.prompt_passwords
            .pop_front()
            .map(SudoPassword::new)
            .context("scripted sudo password source was exhausted")
    }
}

#[derive(Debug)]
struct FakePayloadRemote {
    host: String,
    ssh_statuses: Vec<Vec<String>>,
    rsyncs: Vec<Vec<String>>,
    helper_calls: Vec<Vec<String>>,
}

impl FakePayloadRemote {
    fn new(host: &str) -> Self {
        Self {
            host: host.to_string(),
            ssh_statuses: Vec::new(),
            rsyncs: Vec::new(),
            helper_calls: Vec::new(),
        }
    }
}

impl PayloadSyncRemote for FakePayloadRemote {
    fn remote_host(&self) -> &str {
        &self.host
    }

    fn run_ssh_status<I, S>(&mut self, args: I) -> Result<()>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.ssh_statuses.push(args_to_strings(args));
        Ok(())
    }

    fn run_rsync<I, S>(&mut self, args: I) -> Result<()>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.rsyncs.push(args_to_strings(args));
        Ok(())
    }

    fn run_helper(&mut self, args: &[&str]) -> Result<()> {
        self.helper_calls
            .push(args.iter().map(|arg| (*arg).to_string()).collect());
        Ok(())
    }
}

fn args_to_strings<I, S>(args: I) -> Vec<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    args.into_iter()
        .map(|arg| arg.as_ref().to_string_lossy().into_owned())
        .collect()
}

fn payload_relative_files(root: &Path) -> Result<Vec<String>> {
    let opt = payload_opt(root);
    let mut files = Vec::new();
    collect_relative_files(&opt, &opt, &mut files)?;
    files.sort();
    Ok(files)
}

fn collect_relative_files(base: &Path, dir: &Path, files: &mut Vec<String>) -> Result<()> {
    for entry in fs::read_dir(dir).with_context(|| format!("failed to read {}", dir.display()))? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            collect_relative_files(base, &path, files)?;
        } else if path.is_file() {
            files.push(path.strip_prefix(base)?.to_string_lossy().into_owned());
        }
    }
    Ok(())
}

/// Every `phoxal_cli_test_support` fixture stages its component asset/driver
/// path pins in a `robot.dev.yaml` overlay (path pins are dev-overlay-only
/// in the new grammar); both option builders load it so fixture projects
/// resolve their components without touching a real catalog/network.
fn dry_options() -> DeployOptions {
    DeployOptions {
        host: None,
        dry_run: true,
        target: Some("aarch64".to_string()),
        overlays: vec!["dev".to_string()],
        catalog_source: None,
        health_timeout: Duration::from_secs(3),
    }
}

fn live_options() -> DeployOptions {
    DeployOptions {
        host: Some("robot@test".to_string()),
        dry_run: false,
        target: None,
        overlays: vec!["dev".to_string()],
        catalog_source: None,
        health_timeout: Duration::from_secs(3),
    }
}

#[test]
fn parses_single_deploy_verb_and_rejects_build_pair() {
    let cli = crate::commands::Cli::try_parse_from([
        "phoxal-cli",
        "deploy",
        "--dry-run",
        "--target",
        "aarch64",
    ])
    .expect("deploy dry-run parses");
    let crate::commands::RootCommand::Deploy(command) = cli.command else {
        panic!("expected deploy command");
    };
    assert!(command.dry_run);
    assert_eq!(command.target.as_deref(), Some("aarch64"));

    assert!(crate::commands::Cli::try_parse_from(["phoxal-cli", "deploy", "build"]).is_err());
    assert!(
        crate::commands::Cli::try_parse_from([
            "phoxal-cli",
            "deploy",
            "--dry-run",
            "--target",
            "compose",
        ])
        .is_ok(),
        "clap accepts the value so deploy can emit the designed diagnostic"
    );
}

#[test]
fn target_parser_reserves_update_targets_and_blocks_compose_balena() {
    assert!(target_from_selector("mender").is_err());
    assert!(target_from_selector("rauc").is_err());
    assert!(target_from_selector("compose").is_err());
    assert!(target_from_selector("balena").is_err());
    assert_eq!(
        target_from_selector("aarch64").unwrap().local_triple,
        "aarch64-unknown-linux-musl"
    );
}

#[test]
fn dry_run_renders_units_env_release_and_install_plan() -> Result<()> {
    let _phoxal_home = ScratchPhoxalHome::new()?;
    let temp = tempfile::tempdir()?;
    write_basic_project(temp.path())?;
    let payload = prepare_deploy(
        temp.path(),
        &dry_options(),
        target_for_arch("aarch64"),
        false,
        DRY_RUN_REMOTE_USER,
        &crate::Ui::from_env(),
    )?;
    assert!(
        payload
            .rendered_units
            .contains_key("/etc/systemd/system/phoxal.target")
    );
    assert!(
        payload
            .rendered_units
            .contains_key("/etc/systemd/system/phoxal-router.service")
    );
    let participant_unit = payload
        .rendered_units
        .get("/etc/systemd/system/phoxal-participant-navtask.service")
        .expect("navtask unit rendered");
    assert!(participant_unit.contains("Type=notify"));

    let payload_robot =
        std::fs::read_to_string(payload_opt(payload.root.path()).join("robot.yaml"))?;
    assert!(
        payload_robot.starts_with("schema: robot/v0"),
        "payload robot.yaml must keep the schema tag:\n{payload_robot}"
    );
    phoxal::model::robot::Robot::parse_from_string(&payload_robot)
        .expect("payload robot.yaml must round-trip through the version dispatcher");
    assert!(participant_unit.contains("WatchdogSec=10s"));
    assert!(participant_unit.contains("ExecStart=/opt/phoxal/active/bin/navtask"));
    assert!(
        payload
            .env_files
            .contains_key("/opt/phoxal/active/env/navtask.env")
    );
    assert_eq!(payload.release_json["schema"], RELEASE_SCHEMA);
    let release_artifact_ids = payload.release_json["artifacts"]
        .as_array()
        .expect("release artifacts should be an array")
        .iter()
        .filter_map(|artifact| artifact["id"].as_str())
        .collect::<BTreeSet<_>>();
    assert!(
        release_artifact_ids.contains("phoxal/infrastructure-router"),
        "official infrastructure release record should use package identity: {:?}",
        payload.release_json["artifacts"]
    );
    assert!(payload.install_plan.scoped_delete.is_empty());
    assert_eq!(
        payload.download_descriptor.schema,
        DOWNLOAD_DESCRIPTOR_SCHEMA
    );
    let official = payload.release_json["artifacts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|artifact| artifact["id"] == "phoxal/infrastructure-router")
        .unwrap();
    assert_eq!(official["target"], "aarch64-unknown-linux-gnu");
    assert!(official["url"].as_str().unwrap().starts_with("https://"));
    assert!(
        payload
            .install_plan
            .units
            .contains(&"phoxal-participant-navtask.service".to_string())
    );
    Ok(())
}

#[test]
fn payload_stages_path_component_metadata_and_structures() -> Result<()> {
    let _phoxal_home = ScratchPhoxalHome::new()?;
    let temp = tempfile::tempdir()?;
    phoxal_cli_test_support::write_bench_camera_project(temp.path())?;
    let payload = prepare_deploy(
        temp.path(),
        &dry_options(),
        target_for_arch("aarch64"),
        false,
        DRY_RUN_REMOTE_USER,
        &crate::Ui::from_env(),
    )?;
    let opt = payload_opt(payload.root.path());
    let metadata_files = payload_relative_files(payload.root.path())?
        .into_iter()
        .filter(|path| path == "structure.urdf" || path.starts_with("components/"))
        .collect::<Vec<_>>();

    assert_eq!(
        metadata_files,
        vec![
            "components/bench_camera/component.yaml".to_string(),
            "components/bench_camera/structure.urdf".to_string(),
            "structure.urdf".to_string(),
        ]
    );
    assert!(!opt.join("components/bench_camera/Cargo.toml").exists());
    assert!(!opt.join("components/bench_camera/src").exists());
    assert!(!opt.join("components/bench_camera/target").exists());
    assert!(
        payload
            .install_plan
            .direct_writes
            .contains(&"/opt/phoxal/active/structure.urdf".to_string())
    );
    assert!(
        payload
            .install_plan
            .direct_writes
            .contains(&"/opt/phoxal/active/components/bench_camera/component.yaml".to_string())
    );
    assert!(
        payload
            .install_plan
            .direct_writes
            .contains(&"/opt/phoxal/active/components/bench_camera/structure.urdf".to_string())
    );
    Ok(())
}

#[test]
fn payload_without_path_components_has_no_components_dir() -> Result<()> {
    let _phoxal_home = ScratchPhoxalHome::new()?;
    let temp = tempfile::tempdir()?;
    phoxal_cli_test_support::write_catalog_only_project(temp.path())?;
    // This fixture's component pin is a git (not path) pin with a bogus
    // repository. Deploy metadata staging must skip it without trying
    // `git ls-remote` or `git clone`, so unlike the other fixtures it
    // carries no `robot.dev.yaml` overlay to load.
    let payload = prepare_deploy(
        temp.path(),
        &DeployOptions {
            overlays: Vec::new(),
            ..dry_options()
        },
        target_for_arch("aarch64"),
        false,
        DRY_RUN_REMOTE_USER,
        &crate::Ui::from_env(),
    )?;

    assert!(!payload_opt(payload.root.path()).join("components").exists());
    assert!(
        payload_opt(payload.root.path())
            .join("structure.urdf")
            .is_file()
    );
    Ok(())
}

#[test]
fn sync_payload_stages_opt_tree_and_invokes_install_payload_helper() -> Result<()> {
    let _phoxal_home = ScratchPhoxalHome::new()?;
    let temp = tempfile::tempdir()?;
    write_basic_project(temp.path())?;
    let payload = prepare_deploy(
        temp.path(),
        &dry_options(),
        target_for_arch("aarch64"),
        false,
        DRY_RUN_REMOTE_USER,
        &crate::Ui::from_env(),
    )?;
    let remote_tmp = remote_staging_dir(PAYLOAD_STAGING_PREFIX);
    let mut remote = FakePayloadRemote::new("robot@test");

    sync_payload_via_helper(&mut remote, &payload, &remote_tmp)?;

    assert!(remote_tmp.starts_with(PAYLOAD_STAGING_PREFIX));
    assert_eq!(
        remote.ssh_statuses,
        vec![
            vec!["rm".to_string(), "-rf".to_string(), remote_tmp.clone()],
            vec!["mkdir".to_string(), "-p".to_string(), remote_tmp.clone()],
            vec!["rm".to_string(), "-rf".to_string(), remote_tmp.clone()],
        ]
    );
    assert_eq!(remote.rsyncs.len(), 1);
    let rsync = &remote.rsyncs[0];
    assert_eq!(rsync[0], "-az");
    assert_eq!(rsync[1], "--delete");
    assert_eq!(
        rsync[2],
        payload_opt(payload.root.path())
            .join("")
            .to_string_lossy()
            .into_owned()
    );
    assert_eq!(rsync[3], format!("robot@test:{remote_tmp}/"));
    assert_eq!(
        remote.helper_calls,
        vec![vec![
            "prepare-release".to_string(),
            remote_tmp,
            payload.install_plan.release_generation.clone(),
        ]]
    );
    Ok(())
}

#[test]
fn helper_script_prepare_release_rejects_unsafe_staging_sources() {
    let script = helper_script();

    assert!(script.contains("prepare-release)"), "{script}");
    assert!(
        script.contains("valid_payload_source \"$source\" || exit 64"),
        "{script}"
    );
    assert!(script.contains("/tmp/phoxal-payload-*"), "{script}");
    assert!(
        script.contains("\"\"|*[!A-Za-z0-9_.@-]*) return 1 ;;"),
        "{script}"
    );
    assert!(
        script.contains("suffix=\"${1#/tmp/phoxal-payload-}\""),
        "{script}"
    );
}

#[test]
fn helper_script_prepares_downloads_and_atomically_activates() {
    let script = helper_script();

    assert!(
        script.contains("cp -a \"$source/.\" \"$partial/\""),
        "{script}"
    );
    assert!(script.contains("$expected_sha.partial"), "{script}");
    assert!(script.contains("--retry 2 --retry-all-errors"), "{script}");
    assert!(
        script.contains("[ \"$actual_size\" = \"$expected_size\" ]"),
        "{script}"
    );
    assert!(
        script.contains("[ \"$actual_sha\" = \"$expected_sha\" ]"),
        "{script}"
    );
    assert!(script.contains("mv \"$partial\" \"$archive\""), "{script}");
    assert!(
        script.contains("mv -Tf \"$opt_root/.active.partial\" \"$opt_root/active\""),
        "{script}"
    );
    assert!(script.contains("rollback-release)"), "{script}");
}

#[test]
fn helper_script_restart_target_resets_failed_units_before_restart() {
    let script = helper_script();
    let reset = script
        .find("systemctl reset-failed 'phoxal*' || true")
        .expect("restart-target should reset failed phoxal units");
    let restart = script
        .find("systemctl restart phoxal.target")
        .expect("restart-target should restart phoxal.target");

    assert!(reset < restart, "{script}");
}

#[test]
fn helper_and_stale_cleanup_accept_generated_site_tool_units() {
    let script = helper_script();
    assert!(script.contains("phoxal-tool-*.service"), "{script}");
    assert!(managed_unit_name("phoxal-tool-joypad.service"));
    assert!(managed_unit_name("phoxal-tool-telemetry.service"));
    assert!(!managed_unit_name("phoxal-tool-../escape.service"));

    assert_eq!(
        stale_units(
            &["phoxal-tool-telemetry.service".to_string()],
            &["phoxal.target".to_string()],
        ),
        vec!["phoxal-tool-telemetry.service".to_string()]
    );
}

#[test]
fn helper_script_is_valid_posix_shell() -> Result<()> {
    let mut child = Command::new("sh").arg("-n").stdin(Stdio::piped()).spawn()?;
    child
        .stdin
        .take()
        .context("shell syntax-check stdin missing")?
        .write_all(helper_script().as_bytes())?;
    assert!(child.wait()?.success());
    Ok(())
}

#[test]
fn helper_script_hash_is_stable() {
    assert_eq!(
        helper_script_sha256(),
        "e29c0dc203a8e3bae7d8b93fdf36518a39fe5f85f293f31c15501f15c169c01e"
    );
}

fn test_bootstrap_scripts(remote_user: &str) -> BootstrapScripts {
    BootstrapScripts {
        helper_script: helper_script(),
        sudoers_fragment: sudoers_fragment(),
        remote_user: remote_user.to_string(),
    }
}

#[test]
fn bootstrap_script_creates_phoxal_deploy_group_and_enrolls_remote_user() {
    let script = bootstrap_script(&test_bootstrap_scripts("jetson-op"));
    assert!(
        script.contains("if ! getent group phoxal-deploy >/dev/null; then"),
        "{script}"
    );
    assert!(
        script.contains("groupadd --system phoxal-deploy"),
        "{script}"
    );
    assert!(
        script.contains("usermod -aG phoxal-deploy -- jetson-op"),
        "{script}"
    );
}

#[test]
fn bootstrap_script_terminates_usermod_options_before_remote_user() {
    let script = bootstrap_script(&test_bootstrap_scripts("-operator"));
    assert!(
        script.contains("usermod -aG phoxal-deploy -- -operator"),
        "{script}"
    );
}

#[test]
fn deploy_ssh_commands_disable_connection_multiplexing() {
    let command = deploy_ssh_command();
    assert_eq!(
        command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>(),
        [
            "-o",
            "ControlMaster=no",
            "-o",
            "ControlPath=none",
            "-o",
            "ControlPersist=no",
        ]
    );
}

#[test]
fn bootstrap_script_writes_the_static_group_fragment() {
    let script = bootstrap_script(&test_bootstrap_scripts("jetson-op"));
    assert!(
        script.contains(
            "%phoxal-deploy ALL=(root) NOPASSWD: /usr/local/sbin/phoxal-systemd-helper *"
        ),
        "{script}"
    );
    // The static fragment must not mention the enrolled user by name.
    assert!(!script.contains("jetson-op ALL=(root)"), "{script}");
}

#[test]
fn bootstrap_script_is_valid_posix_shell() -> Result<()> {
    let script = bootstrap_script(&test_bootstrap_scripts("jetson-op"));
    let mut child = Command::new("sh").arg("-n").stdin(Stdio::piped()).spawn()?;
    child
        .stdin
        .take()
        .context("shell syntax-check stdin missing")?
        .write_all(script.as_bytes())?;
    assert!(child.wait()?.success(), "{script}");
    Ok(())
}

#[test]
fn validate_remote_username_accepts_conservative_charset() {
    for user in ["robot", "jetson-op", "user.name", "user_name", "a@b-c.d"] {
        assert!(
            validate_remote_username(user).is_ok(),
            "{user} should be accepted"
        );
    }
}

#[test]
fn validate_remote_username_rejects_hostile_input() {
    for user in [
        "",
        "evil'; rm -rf /",
        "user name",
        "user$(whoami)",
        "user\nrm -rf /",
        "user;ls",
        "user`ls`",
    ] {
        let error = validate_remote_username(user)
            .err()
            .unwrap_or_else(|| panic!("{user:?} should be rejected"));
        assert!(
            error.to_string().contains("DeployInvalidRemoteUser"),
            "{error}"
        );
    }
}

#[test]
fn render_payload_rejects_a_hostile_remote_user() {
    let _phoxal_home = ScratchPhoxalHome::new().expect("scratch phoxal home");
    let temp = tempfile::tempdir().expect("tempdir");
    write_basic_project(temp.path()).expect("write project");
    let mut transport = FakeTransport::healthy();
    transport.probe.remote_user = "evil'; rm -rf /".to_string();
    let error = deploy_with_transport(
        temp.path(),
        &live_options(),
        &mut transport,
        false,
        &crate::Ui::from_env(),
    )
    .expect_err("a hostile remote user must be rejected before bootstrapping anything");
    assert!(
        error.to_string().contains("DeployInvalidRemoteUser"),
        "{error}"
    );
    assert!(
        !transport.bootstrapped,
        "bootstrap must never run with an unvalidated remote user"
    );
}

#[test]
fn download_executor_is_bounded_and_processes_every_artifact() -> Result<()> {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let active = AtomicUsize::new(0);
    let maximum = AtomicUsize::new(0);
    let completed = AtomicUsize::new(0);
    let artifacts = (0..11).collect::<Vec<_>>();
    run_bounded(&artifacts, DOWNLOAD_CONCURRENCY, |_| {
        let now = active.fetch_add(1, Ordering::SeqCst) + 1;
        maximum.fetch_max(now, Ordering::SeqCst);
        std::thread::yield_now();
        completed.fetch_add(1, Ordering::SeqCst);
        active.fetch_sub(1, Ordering::SeqCst);
        Ok(())
    })?;

    assert_eq!(completed.load(Ordering::SeqCst), artifacts.len());
    assert!(maximum.load(Ordering::SeqCst) <= DOWNLOAD_CONCURRENCY);
    Ok(())
}

#[test]
fn driver_graph_renders_one_unit_per_instance_with_privileges() -> Result<()> {
    let _phoxal_home = ScratchPhoxalHome::new()?;
    let temp = tempfile::tempdir()?;
    phoxal_cli_test_support::write_driver_project(temp.path())?;
    let payload = prepare_deploy(
        temp.path(),
        &dry_options(),
        target_for_arch("aarch64"),
        false,
        DRY_RUN_REMOTE_USER,
        &crate::Ui::from_env(),
    )?;
    let left = payload
        .rendered_units
        .get("/etc/systemd/system/phoxal-participant-left_drive.service")
        .expect("left unit");
    let right = payload
        .rendered_units
        .get("/etc/systemd/system/phoxal-participant-right_drive.service")
        .expect("right unit");
    assert!(left.contains("DeviceAllow=/dev/ttyUSB0 rw"));
    assert!(left.contains("SupplementaryGroups=dialout"));
    assert!(right.contains("DeviceAllow=/dev/i2c-1 rw"));
    assert!(right.contains("SupplementaryGroups=i2c"));
    assert!(left.contains("ExecStart=/opt/phoxal/active/bin/driver-ddsm115"));
    assert!(right.contains("ExecStart=/opt/phoxal/active/bin/driver-ddsm115"));
    Ok(())
}

fn resolved_with_components(
    components: Vec<phoxal_cli_core::project::resolver::ResolvedComponent>,
) -> Result<ResolvedRobot> {
    Ok(ResolvedRobot {
        robot: Robot::parse_from_string(MINIMAL_RESOLVED_ROBOT_YAML)?,
        channel: phoxal_cli_core::project::catalog::SelectionChannel::Stable,
        target: crate::resolver::host_target_triple(),
        catalog_snapshot: None,
        platform_runtimes: Vec::new(),
        simulators: Vec::new(),
        user_runtimes: Vec::new(),
        components,
        tools: Vec::new(),
        path_overrides: Vec::new(),
    })
}

const MINIMAL_RESOLVED_ROBOT_YAML: &str = r#"schema: robot/v0
robot:
  id: testbot
  namespace: test
  motion_limits:
    max_linear_speed_mps: 0.6
    max_angular_speed_radps: 2.0
  kinematic:
    kind: omnidirectional
    actuators: []
    encoders: []
  components: {}
"#;

/// A Catalog-sourced component package with a populated `catalog_runtime`
/// but nothing warmed in the local artifact cache - the shape a fresh
/// clean machine sees before any `phoxal update`.
fn cold_cache_catalog_component_package(
    package: &str,
    kind: phoxal_cli_core::project::catalog::ArtifactKind,
    component_name: &str,
) -> phoxal_cli_core::project::resolver::ResolvedComponentPackage {
    phoxal_cli_core::project::resolver::ResolvedComponentPackage {
        package: package.to_string(),
        kind,
        source: ResolvedComponentSource::Catalog,
        path_override: None,
        catalog_runtime: Some(ResolvedPlatformRuntime {
            name: component_name.to_string(),
            package: package.to_string(),
            kind,
            version: "0.1.0".to_string(),
            artifact_ref: format!(
                "phoxal-component-{component_name}-{}-v0.1.0-aarch64-unknown-linux-gnu.tar.zst",
                kind.catalog_kind()
            ),
            sha256: Some("a".repeat(64)),
            url: Some("https://example.invalid/component.tar.zst".to_string()),
            size: Some(1),
            published: true,
            published_triples: Vec::new(),
            path_override: None,
            channel: phoxal_cli_core::project::catalog::SelectionChannel::Stable,
            target: Some("aarch64-unknown-linux-gnu".to_string()),
        }),
    }
}

/// Selects a scratch project-local store so this test's "nothing vendored
/// yet" assumption is exact. Shared crate-wide because the project-root
/// test override is process-global and unit tests run concurrently.
use crate::host_paths::test_support::ScratchPhoxalHome;

#[test]
fn dry_run_stays_offline_for_catalog_resolved_component_driver() -> Result<()> {
    // Band B kept `deploy --dry-run` from resolving git component
    // commits so it never touches the network; a Catalog-sourced
    // component driver/assets pair must uphold the identical guarantee.
    // This exercises exactly the two functions `render_payload` calls to
    // stage a component's driver binary / assets bundle
    // (`stage_official_artifacts`'s runtime lookup and
    // `locate_cached_component_assets_dir`) directly against a cold
    // cache, bypassing the graph-check emit-apis fetch (a pre-existing,
    // symmetric-with-services network dependency that is unrelated to
    // this staging step and out of this change's scope). Observing a
    // clean, local-only result - `NativePending`-eligible missing
    // binary, no cached assets dir - proves neither function reaches
    // for the network; this process has real internet access, so a
    // download attempt against the fixture's made-up (unpublished)
    // asset name would surface as a loud HTTP/connection error, not a
    // silent hang.
    let _phoxal_home = ScratchPhoxalHome::new()?;

    let driver_package = cold_cache_catalog_component_package(
        "phoxal/component-ddsm115",
        phoxal_cli_core::project::catalog::ArtifactKind::ComponentDriver,
        "ddsm115",
    );
    let assets_package = cold_cache_catalog_component_package(
        "phoxal/component-ddsm115",
        phoxal_cli_core::project::catalog::ArtifactKind::ComponentAssets,
        "ddsm115",
    );

    let mut resolved = resolved_with_components(vec![ResolvedComponent {
        instance: "left_drive".to_string(),
        source_name: "ddsm115".to_string(),
        assets: Some(assets_package.clone()),
        driver: Some(driver_package),
        has_driver: true,
    }])?;
    resolved.tools.push(ResolvedTool {
        kind: phoxal_cli_core::project::catalog::ArtifactKind::Infrastructure,
        name: SITE_INFRASTRUCTURE_ROUTER.to_string(),
        package: "phoxal/infrastructure-router".to_string(),
        requested: "0.1.0".to_string(),
        resolved: "0.1.0".to_string(),
        repo: "phoxal/framework".to_string(),
        asset: "phoxal-infrastructure-router-0.1.0-aarch64-unknown-linux-gnu.tar.zst".to_string(),
        binary_name: "phoxal-infrastructure-router".to_string(),
        sha256: "0".repeat(64),
        url: None,
        size: None,
        published: true,
        path_override: Some(PathBuf::from("/fake/router")),
        channel: phoxal_cli_core::project::catalog::SelectionChannel::Stable,
        target: "aarch64-unknown-linux-gnu".to_string(),
    });

    // 1) `official_runtime_by_artifact_id` finds the driver's
    //    `catalog_runtime` (proving it is visible through the same
    //    lookup a service uses).
    let found = official_runtime_by_artifact_id(&resolved, "ddsm115")
        .expect("catalog driver runtime must be discoverable by its artifact id");
    assert_eq!(found.package, "phoxal/component-ddsm115");

    // 2) `official_runtime_plan` (what `stage_official_artifacts` calls)
    //    reports the binary as locally absent rather than downloading -
    //    a cold cache yields `source_path: None`, which the caller
    //    turns into `NativePending` for a live deploy or a tolerated
    //    "missing" entry for `--dry-run`.
    let root = tempfile::tempdir()?;
    let plan = official_runtime_plan(root.path(), found, false)?;
    assert!(
        plan.source_path.is_none(),
        "a cold cache must report no local binary, not download one"
    );
    assert!(plan.missing_label.is_none(), "artifact is published");

    // 3) `locate_cached_component_assets_dir` returns `None` on a cold
    //    cache instead of fetching the assets bundle.
    assert_eq!(
        locate_cached_component_assets_dir(&assets_package)?,
        None,
        "a cold cache must report no cached assets dir, not download one"
    );

    Ok(())
}

/// CLI-UX Phase 4: deploy now ships EVERY standard site tool
/// (`tool-router`, `tool-joypad`, `tool-telemetry`), not just the
/// router - each gets its own unit ordered after the router, and
/// `tool-joypad` carries the `/dev/input` tool-privilege grant
/// (`unit_privileges_for_tool`). `write_basic_project`'s fixture catalog
/// auto-fills every `OFFICIAL_TOOLS`/`OFFICIAL_OPTIONAL_TOOLS` entry
/// (`catalog::fixture_catalog_for_tests`), so all three resolve here.
#[test]
fn privileged_tool_graph_renders_router_joypad_and_telemetry_units() -> Result<()> {
    let _phoxal_home = ScratchPhoxalHome::new()?;
    let temp = tempfile::tempdir()?;
    write_basic_project(temp.path())?;
    let payload = prepare_deploy(
        temp.path(),
        &dry_options(),
        target_for_arch("aarch64"),
        false,
        DRY_RUN_REMOTE_USER,
        &crate::Ui::from_env(),
    )?;
    assert!(
        payload
            .rendered_units
            .contains_key("/etc/systemd/system/phoxal-router.service")
    );
    let joypad_unit = payload
        .rendered_units
        .get("/etc/systemd/system/phoxal-tool-joypad.service")
        .expect("tool-joypad unit must render");
    assert!(joypad_unit.contains("After=network-online.target phoxal-router.service"));
    assert!(joypad_unit.contains("SupplementaryGroups=input"));
    assert!(joypad_unit.contains("DeviceAllow=/dev/input/* rw"));
    let telemetry_unit = payload
        .rendered_units
        .get("/etc/systemd/system/phoxal-tool-telemetry.service")
        .expect("tool-telemetry unit must render");
    assert!(telemetry_unit.contains("After=network-online.target phoxal-router.service"));
    assert!(!telemetry_unit.contains("SupplementaryGroups="));
    assert!(!telemetry_unit.contains("DeviceAllow="));
    assert!(
        !payload
            .env_files
            .contains_key("/opt/phoxal/active/env/router.env")
    );
    for env_name in ["tool-joypad.env", "tool-telemetry.env"] {
        let contents = payload
            .env_files
            .get(&format!("/opt/phoxal/active/env/{env_name}"))
            .unwrap_or_else(|| panic!("{env_name} must render"));
        assert!(
            !contents.contains("PHOXAL_CLOCK="),
            "tools must not receive a clock selection in {env_name}:\n{contents}"
        );
    }
    // Stale-unit cleanup and unit installation both key off `unit_names`
    // generically - proving the new units are IN that list (not just
    // rendered as file content) is what actually wires them into
    // install/health-check/restart.
    assert!(
        payload
            .unit_names
            .contains(&"phoxal-tool-joypad.service".to_string())
    );
    assert!(
        payload
            .unit_names
            .contains(&"phoxal-tool-telemetry.service".to_string())
    );
    Ok(())
}

#[test]
fn rejected_non_immutable_artifact_gets_designed_error() -> Result<()> {
    let _phoxal_home = ScratchPhoxalHome::new()?;
    let temp = tempfile::tempdir()?;
    phoxal_cli_test_support::write_native_dep_project(temp.path())?;
    let error = prepare_deploy(
        temp.path(),
        &dry_options(),
        target_for_arch("aarch64"),
        false,
        DRY_RUN_REMOTE_USER,
        &crate::Ui::from_env(),
    )
    .expect_err("native C deps should be rejected before raw linker spew");
    let message = error.to_string();
    assert!(message.contains("CrossBuildUnsupported"), "{message}");
    assert!(message.contains("opencv"), "{message}");
    Ok(())
}

#[test]
#[cfg(unix)]
fn missing_zig_toolchain_path_gets_designed_fix() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let bin = temp.path().join("bin");
    fs::create_dir_all(&bin)?;
    write_test_executable(&bin.join("cargo"), "#!/bin/sh\nexit 0\n")?;
    let base = OsString::from("/definitely-not-on-path");
    let search_path = path_with_cache_bin(&bin, Some(base.as_os_str()))?;

    let error = validate_zigbuild_toolchain(&search_path, &bin)
        .expect_err("missing zig should be diagnosed before build");
    let message = error.to_string();

    assert!(message.contains("CrossBuildToolchainMissing"), "{message}");
    assert!(message.contains("zig is required"), "{message}");
    assert!(message.contains("brew install zig"), "{message}");
    Ok(())
}

#[test]
#[cfg(unix)]
fn missing_cargo_zigbuild_path_gets_designed_fix() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let bin = temp.path().join("bin");
    fs::create_dir_all(&bin)?;
    write_test_executable(&bin.join("cargo"), "#!/bin/sh\nexit 1\n")?;
    write_test_executable(&bin.join("zig"), "#!/bin/sh\nexit 0\n")?;
    let base = OsString::from("/definitely-not-on-path");
    let search_path = path_with_cache_bin(&bin, Some(base.as_os_str()))?;

    let error = validate_zigbuild_toolchain(&search_path, &bin)
        .expect_err("missing cargo-zigbuild should be diagnosed before build");
    let message = error.to_string();

    assert!(message.contains("CrossBuildToolchainMissing"), "{message}");
    assert!(message.contains("cargo-zigbuild 0.23.0"), "{message}");
    assert!(
        message.contains("cargo install cargo-zigbuild --locked --version 0.23.0"),
        "{message}"
    );
    Ok(())
}

#[test]
fn zigbuild_failure_classifies_native_sysroot_crate() {
    let message = classify_zigbuild_failure(
        "vision",
        "aarch64-unknown-linux-musl",
        b"",
        b"error: failed to run custom build command for `opencv v0.92.0`\n\
              pkg-config has not been configured to support cross-compilation\n",
    );

    assert!(message.contains("CrossBuildUnsupported"), "{message}");
    assert!(message.contains("opencv"), "{message}");
    assert!(
        message.contains("target-native system headers/libs"),
        "{message}"
    );
}

#[test]
fn stale_unit_removal_is_computed_by_tree_comparison() -> Result<()> {
    let _phoxal_home = ScratchPhoxalHome::new()?;
    let temp = tempfile::tempdir()?;
    write_basic_project(temp.path())?;
    let mut transport = FakeTransport::healthy();
    transport.installed_units = vec![
        "phoxal.target".to_string(),
        "phoxal-router.service".to_string(),
        "phoxal-participant-old.service".to_string(),
    ];
    let report = deploy_with_transport(
        temp.path(),
        &live_options(),
        &mut transport,
        false,
        &crate::Ui::from_env(),
    )?;
    assert_eq!(
        transport.stale_removed,
        vec!["phoxal-participant-old.service"]
    );
    assert!(
        report
            .install_plan
            .stale_units_to_remove
            .contains(&"phoxal-participant-old.service".to_string())
    );
    assert!(transport.bootstrapped);
    assert!(transport.synced);
    assert!(transport.restarted);
    assert!(!transport.downloaded.is_empty());
    assert!(!transport.fallback_prepared);
    assert!(transport.activated.is_some());
    assert!(transport.finalized);
    assert_eq!(
        transport.previous_generation.as_deref(),
        Some("releases/previous-generation")
    );
    assert_eq!(report.delivery, Some(OfficialDelivery::RobotDownload));
    Ok(())
}

#[test]
fn unreachable_github_uses_host_transfer_fallback() -> Result<()> {
    let _phoxal_home = ScratchPhoxalHome::new()?;
    let temp = tempfile::tempdir()?;
    write_basic_project(temp.path())?;
    let mut transport = FakeTransport::healthy();
    transport.github_reachable = false;

    let report = deploy_with_transport(
        temp.path(),
        &live_options(),
        &mut transport,
        false,
        &crate::Ui::from_env(),
    )?;

    assert!(transport.fallback_prepared);
    assert!(transport.downloaded.is_empty());
    assert!(transport.activated.is_some());
    assert_eq!(
        report.delivery,
        Some(OfficialDelivery::HostTransferFallback)
    );
    Ok(())
}

#[test]
fn failed_artifact_verification_never_activates() -> Result<()> {
    let _phoxal_home = ScratchPhoxalHome::new()?;
    let temp = tempfile::tempdir()?;
    write_basic_project(temp.path())?;
    let mut transport = FakeTransport::healthy();
    transport.download_fails = true;

    let error = deploy_with_transport(
        temp.path(),
        &live_options(),
        &mut transport,
        false,
        &crate::Ui::from_env(),
    )
    .expect_err("verification failure must abort before activation");

    assert!(error.to_string().contains("robot failed to download"));
    assert!(transport.activated.is_none());
    assert!(!transport.restarted);
    assert!(!transport.rolled_back);
    Ok(())
}

#[cfg(unix)]
fn write_test_executable(path: &Path, contents: &str) -> Result<()> {
    fs::write(path, contents)?;
    make_executable(path)?;
    Ok(())
}

fn probe(bootstrap_required: bool, sudo_noninteractive: bool, helper_grant: bool) -> RemoteProbe {
    probe_with_helper_stale(bootstrap_required, sudo_noninteractive, helper_grant, false)
}

fn probe_with_helper_stale(
    bootstrap_required: bool,
    sudo_noninteractive: bool,
    helper_grant: bool,
    helper_stale: bool,
) -> RemoteProbe {
    RemoteProbe {
        arch: "aarch64".to_string(),
        bootstrap_required,
        remote_user: "robot".to_string(),
        sudo_noninteractive,
        helper_grant,
        helper_stale,
    }
}

#[test]
fn sudo_probe_row1_noninteractive_sudo_always_proceeds() {
    // sudo -n true works: proceed regardless of bootstrap/grant state or
    // local tty - any root work runs non-interactively and no password
    // source is touched.
    for probe in [
        probe(true, true, true),
        probe(false, true, true),
        probe(false, true, false),
    ] {
        let mut transport = FakeTransport::healthy();
        let mut source = ScriptedSudoPasswordSource::none();
        let password =
            ensure_sudo_will_succeed("robot@jetson", &probe, false, &mut source, &mut transport)
                .expect("row 1 should proceed");
        assert!(password.is_none());
        assert_eq!(source.env_calls, 0);
        assert_eq!(source.prompt_calls, 0);
        assert!(transport.validation_password_stdin.is_empty());
    }
}

#[test]
fn sudo_probe_row2_helper_grant_for_this_user_proceeds_without_tty() {
    // No blanket sudo, but the installed helper's per-command grant
    // covers this user and the helper hash matches: steady-state deploy,
    // no root work needed and no password source is touched.
    let probe = probe(false, false, true);
    let mut transport = FakeTransport::healthy();
    let mut source = ScriptedSudoPasswordSource::none();
    let password =
        ensure_sudo_will_succeed("robot@jetson", &probe, false, &mut source, &mut transport)
            .expect("row 2 should proceed");
    assert!(password.is_none());
    assert_eq!(source.env_calls, 0);
    assert_eq!(source.prompt_calls, 0);
    assert!(transport.validation_password_stdin.is_empty());
}

#[test]
fn sudo_probe_row3_root_work_with_tty_prompts_and_validates() {
    // Root work required (first bootstrap, or stale grant repair) and
    // local /dev/tty is available: read one password and validate it now.
    let probe = probe(true, false, false);
    let mut transport = FakeTransport::healthy();
    let mut source = ScriptedSudoPasswordSource::with_prompts(&["secret"]);
    let password =
        ensure_sudo_will_succeed("robot@jetson", &probe, true, &mut source, &mut transport)
            .expect("row 3 should proceed after a valid password");

    assert!(password.is_some());
    assert_eq!(source.env_calls, 1);
    assert_eq!(source.prompt_calls, 1);
    assert_eq!(
        source.prompts_seen,
        vec!["[sudo] password for robot on robot@jetson:".to_string()]
    );
    assert_eq!(
        transport.validation_password_stdin,
        vec![b"secret\n".to_vec()]
    );
}

#[test]
fn sudo_probe_validation_failure_retries_once_then_errors() {
    let probe = probe(true, false, false);
    let mut transport = FakeTransport::healthy();
    transport.validation_results = VecDeque::from([false, false]);
    let mut source = ScriptedSudoPasswordSource::with_prompts(&["bad", "still-bad"]);

    let error = ensure_sudo_will_succeed("robot@jetson", &probe, true, &mut source, &mut transport)
        .err()
        .expect("two failed sudo validations should stop deploy");
    let message = error.to_string();

    assert!(message.contains("DeploySudoPasswordRejected"), "{message}");
    assert!(message.contains("robot@jetson"), "{message}");
    assert_eq!(source.prompt_calls, 2);
    assert_eq!(
        transport.validation_password_stdin,
        vec![b"bad\n".to_vec(), b"still-bad\n".to_vec()]
    );
    assert!(!transport.bootstrapped);
}

#[test]
fn sudo_probe_env_password_without_tty_proceeds() {
    let probe = probe(true, false, false);
    let mut transport = FakeTransport::healthy();
    let mut source = ScriptedSudoPasswordSource::with_env("env-secret");
    let password =
        ensure_sudo_will_succeed("robot@jetson", &probe, false, &mut source, &mut transport)
            .expect("env password should satisfy root work without a tty");

    assert!(password.is_some());
    assert_eq!(source.env_calls, 1);
    assert_eq!(source.prompt_calls, 0);
    assert_eq!(
        transport.validation_password_stdin,
        vec![b"env-secret\n".to_vec()]
    );
}

#[test]
fn row3_deploy_bootstrap_uses_sudo_s_and_writes_password_once() -> Result<()> {
    let _phoxal_home = ScratchPhoxalHome::new()?;
    let temp = tempfile::tempdir()?;
    write_basic_project(temp.path())?;
    let mut transport = FakeTransport::healthy();
    transport.probe.sudo_noninteractive = false;
    transport.probe.helper_grant = false;
    let mut source = ScriptedSudoPasswordSource::with_prompts(&["secret"]);

    deploy_with_transport_with_sudo(
        temp.path(),
        &live_options(),
        &mut transport,
        true,
        &mut source,
        &crate::Ui::from_env(),
    )?;

    assert!(transport.bootstrapped);
    assert_eq!(source.prompt_calls, 1);
    assert_eq!(
        transport.validation_password_stdin,
        vec![b"secret\n".to_vec()]
    );
    assert_eq!(
        transport.bootstrap_sudo_command_seen,
        Some(vec![
            "sudo".to_string(),
            "-S".to_string(),
            "-p".to_string(),
            SUDO_STDIN_PROMPT.to_string(),
            "sh".to_string(),
            "/tmp/phoxal-bootstrap.TEST.sh".to_string(),
        ])
    );
    assert_eq!(
        transport.bootstrap_password_stdin,
        vec![b"secret\n".to_vec()]
    );
    Ok(())
}

#[test]
fn sudo_probe_row4_root_work_without_tty_fails_fast() {
    // Root work required and no local tty: fail before building
    // anything, naming the host and all remedies.
    let probe = probe(true, false, false);
    let mut transport = FakeTransport::healthy();
    let mut source = ScriptedSudoPasswordSource::none();
    let error =
        ensure_sudo_will_succeed("robot@jetson", &probe, false, &mut source, &mut transport)
            .err()
            .expect("non-interactive sudo with required bootstrap must fail fast");
    let message = error.to_string();
    assert!(message.contains("DeploySudoRequiresPassword"), "{message}");
    assert!(message.contains("robot@jetson"), "{message}");
    assert!(message.contains("robot"), "{message}");
    assert!(message.contains("first deploy"), "{message}");
    assert!(message.contains("interactively"), "{message}");
    assert!(message.contains("NOPASSWD"), "{message}");
    assert!(message.contains(SUDO_PASSWORD_ENV), "{message}");
}

#[test]
fn sudo_probe_row4_stale_grant_without_tty_fails_fast_naming_repair() {
    // Bootstrapped host, but this user is not covered by the group grant
    // (`sudo -n true` fails and the helper grant probe fails):
    // blanket-sudo success must not be inferred from the helper being
    // installed - fail fast and name the group-grant repair rather than
    // the first install.
    let probe = probe(false, false, false);
    let mut transport = FakeTransport::healthy();
    let mut source = ScriptedSudoPasswordSource::none();
    let error =
        ensure_sudo_will_succeed("robot@jetson", &probe, false, &mut source, &mut transport)
            .err()
            .expect("stale helper grant without a tty must fail fast, not die mid-flight");
    let message = error.to_string();
    assert!(message.contains("DeploySudoRequiresPassword"), "{message}");
    assert!(message.contains("robot@jetson"), "{message}");
    assert!(
        message.contains("not covered by the phoxal-deploy group grant"),
        "{message}"
    );
    assert!(message.contains("add this user to the group"), "{message}");
    assert!(!message.contains("first deploy"), "{message}");
    assert!(message.contains("NOPASSWD"), "{message}");
}

#[test]
fn sudo_probe_row4_stale_helper_without_tty_fails_fast_naming_repair() {
    let probe = probe_with_helper_stale(false, false, true, true);
    let mut transport = FakeTransport::healthy();
    let mut source = ScriptedSudoPasswordSource::none();
    let error =
        ensure_sudo_will_succeed("robot@jetson", &probe, false, &mut source, &mut transport)
            .err()
            .expect("stale helper without a tty must fail fast");
    let message = error.to_string();
    assert!(message.contains("DeploySudoRequiresPassword"), "{message}");
    assert!(message.contains("stale"), "{message}");
    assert!(message.contains("rewrite the helper"), "{message}");
    assert!(!message.contains("first deploy"), "{message}");
}

#[test]
fn sudoers_fragment_is_the_static_group_grant() {
    let fragment = sudoers_fragment();
    assert_eq!(
        fragment,
        "%phoxal-deploy ALL=(root) NOPASSWD: /usr/local/sbin/phoxal-systemd-helper *\n"
    );
    // Calling it again (as a second operator's deploy would) must
    // produce the identical fragment - nobody's grant gets revoked by
    // someone else's deploy.
    assert_eq!(fragment, sudoers_fragment());
}

#[test]
fn deploy_with_transport_writes_static_fragment_and_enrolls_probed_user() -> Result<()> {
    let _phoxal_home = ScratchPhoxalHome::new()?;
    let temp = tempfile::tempdir()?;
    write_basic_project(temp.path())?;
    let mut transport = FakeTransport::healthy();
    transport.probe.remote_user = "jetson-op".to_string();
    deploy_with_transport(
        temp.path(),
        &live_options(),
        &mut transport,
        false,
        &crate::Ui::from_env(),
    )?;
    assert!(transport.bootstrapped);
    let fragment = transport
        .bootstrap_fragment_seen
        .expect("bootstrap should have been called with a fragment");
    assert_eq!(
        fragment,
        "%phoxal-deploy ALL=(root) NOPASSWD: /usr/local/sbin/phoxal-systemd-helper *\n"
    );
    assert_eq!(
        transport.bootstrap_remote_user_seen,
        Some("jetson-op".to_string())
    );
    Ok(())
}

#[test]
fn stale_helper_grant_triggers_bootstrap_repair_over_tty() -> Result<()> {
    // Bootstrapped host (bootstrap_required false), but the grant probe
    // failed - e.g. this user has never deployed to this host, or the
    // host is still on the old per-user model. With a local tty the
    // deploy must re-run the bootstrap script (it rewrites the helper
    // and the fragment idempotently) and enroll the new deploying user
    // into the phoxal-deploy group.
    let _phoxal_home = ScratchPhoxalHome::new()?;
    let temp = tempfile::tempdir()?;
    write_basic_project(temp.path())?;
    let mut transport = FakeTransport::healthy();
    transport.probe.bootstrap_required = false;
    transport.probe.sudo_noninteractive = false;
    transport.probe.helper_grant = false;
    transport.probe.remote_user = "user-b".to_string();
    let mut source = ScriptedSudoPasswordSource::with_prompts(&["secret"]);
    deploy_with_transport_with_sudo(
        temp.path(),
        &live_options(),
        &mut transport,
        true,
        &mut source,
        &crate::Ui::from_env(),
    )?;
    assert!(
        transport.bootstrapped,
        "a missing/stale grant must re-run bootstrap even though /opt/phoxal exists"
    );
    let fragment = transport
        .bootstrap_fragment_seen
        .expect("bootstrap should have been called with a fragment");
    assert_eq!(
        fragment,
        "%phoxal-deploy ALL=(root) NOPASSWD: /usr/local/sbin/phoxal-systemd-helper *\n"
    );
    assert_eq!(
        transport.bootstrap_remote_user_seen,
        Some("user-b".to_string())
    );
    Ok(())
}

#[test]
fn blanket_sudo_without_group_membership_still_triggers_bootstrap_repair() -> Result<()> {
    // A device with blanket passwordless sudo but no phoxal-deploy
    // membership (e.g. deployed under the old per-user model) must
    // still take the bootstrap/repair path so it converges to the group
    // model. Because sudo_noninteractive is true, this proceeds and
    // runs the repair non-interactively, without touching the local tty
    // or a password source at all.
    let _phoxal_home = ScratchPhoxalHome::new()?;
    let temp = tempfile::tempdir()?;
    write_basic_project(temp.path())?;
    let mut transport = FakeTransport::healthy();
    transport.probe.bootstrap_required = false;
    transport.probe.sudo_noninteractive = true;
    transport.probe.helper_grant = false;
    transport.probe.remote_user = "legacy-user".to_string();
    let mut source = ScriptedSudoPasswordSource::none();
    deploy_with_transport_with_sudo(
        temp.path(),
        &live_options(),
        &mut transport,
        false,
        &mut source,
        &crate::Ui::from_env(),
    )?;
    assert!(
        transport.bootstrapped,
        "blanket sudo without phoxal-deploy membership must still repair, not skip bootstrap"
    );
    assert_eq!(source.env_calls, 0);
    assert_eq!(source.prompt_calls, 0);
    assert_eq!(
        transport.bootstrap_remote_user_seen,
        Some("legacy-user".to_string())
    );
    Ok(())
}

#[test]
fn stale_helper_hash_triggers_bootstrap_repair_with_existing_grant() -> Result<()> {
    let _phoxal_home = ScratchPhoxalHome::new()?;
    let temp = tempfile::tempdir()?;
    write_basic_project(temp.path())?;
    let mut transport = FakeTransport::healthy();
    transport.probe.bootstrap_required = false;
    transport.probe.sudo_noninteractive = false;
    transport.probe.helper_grant = true;
    transport.probe.helper_stale = true;
    let mut source = ScriptedSudoPasswordSource::with_prompts(&["secret"]);
    deploy_with_transport_with_sudo(
        temp.path(),
        &live_options(),
        &mut transport,
        true,
        &mut source,
        &crate::Ui::from_env(),
    )?;
    assert!(
        transport.bootstrapped,
        "a stale helper must re-run bootstrap even when the helper grant is valid"
    );
    Ok(())
}

#[test]
fn failed_health_push_exits_nonzero_with_diagnosis() -> Result<()> {
    let _phoxal_home = ScratchPhoxalHome::new()?;
    let temp = tempfile::tempdir()?;
    write_basic_project(temp.path())?;
    let mut transport = FakeTransport::healthy();
    transport.health = HealthReport {
        units: vec![HealthUnitReport {
            unit: "phoxal-participant-navtask.service".to_string(),
            participant: Some("navtask".to_string()),
            ready: false,
            active_state: "failed".to_string(),
            sub_state: "failed".to_string(),
            journal_excerpt: vec!["boom".to_string()],
        }],
    };
    let error = deploy_with_transport(
        temp.path(),
        &live_options(),
        &mut transport,
        false,
        &crate::Ui::from_env(),
    )
    .expect_err("health failure should fail deploy");
    let message = error.to_string();
    assert!(message.contains("HealthReportFailed"), "{message}");
    assert!(message.contains("navtask"), "{message}");
    assert!(message.contains("boom"), "{message}");
    assert!(message.contains("rolled back"), "{message}");
    assert!(transport.rolled_back);
    assert!(!transport.finalized);
    assert_eq!(
        transport.active_generation.as_deref(),
        Some("releases/previous-generation")
    );
    assert!(
        transport
            .previous_generation
            .as_deref()
            .is_some_and(|generation| generation != "releases/previous-generation")
    );
    Ok(())
}
