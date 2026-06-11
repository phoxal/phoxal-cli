use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};

use crate::AppContext;
use crate::catalog::{DEFAULT_RUNTIME_VERSION, SUPPORTED_RUNTIME_TRAIN};
use crate::lockfile::{LOCKFILE_NAME, Lockfile};

#[derive(Debug, Subcommand)]
pub enum Create {
    #[command(about = "Scaffold a robot project.")]
    Robot(Robot),
    #[command(about = "Scaffold a user runtime under the current robot.")]
    Runtime(Runtime),
    #[command(about = "Scaffold a custom component under the current robot.")]
    Component(Component),
    #[command(about = "Scaffold a Phoxal catalog component repository skeleton.")]
    CatalogComponent(CatalogComponent),
}

#[derive(Debug, Args)]
pub struct Robot {
    pub id: String,
    #[arg(long, default_value = ".")]
    pub dir: PathBuf,
}

#[derive(Debug, Args)]
pub struct Runtime {
    pub name: String,
}

#[derive(Debug, Args)]
pub struct Component {
    pub name: String,
}

#[derive(Debug, Args)]
pub struct CatalogComponent {
    pub gtin: String,
    #[arg(long)]
    pub dir: Option<PathBuf>,
}

impl Create {
    pub async fn run(&self, app: &AppContext) -> Result<()> {
        match self {
            Self::Robot(command) => create_robot(app, command),
            Self::Runtime(command) => create_runtime(app, command),
            Self::Component(command) => create_component(app, command),
            Self::CatalogComponent(command) => create_catalog_component(app, command),
        }
    }
}

fn create_robot(app: &AppContext, command: &Robot) -> Result<()> {
    let root = if command.dir.is_absolute() {
        command.dir.clone()
    } else {
        app.project.root().join(&command.dir)
    };
    fs::create_dir_all(&root).with_context(|| format!("failed to create {}", root.display()))?;
    write_new(
        root.join("robot.yaml"),
        robot_yaml(&command.id, SUPPORTED_RUNTIME_TRAIN),
    )?;
    write_new(
        root.join("structure.urdf"),
        format!(
            r#"<?xml version="1.0"?>
<robot name="{}">
  <link name="base_link"/>
</robot>
"#,
            command.id
        ),
    )?;
    fs::create_dir_all(root.join("worlds"))?;
    write_new(
        root.join("worlds/default.wbt"),
        "#VRML_SIM R2025a utf8\n".to_string(),
    )?;
    app.ui.success(format!("created robot {}", root.display()));
    Ok(())
}

/// Body of the scaffolded `robot.yaml`.
///
/// The differential drive lists are seeded with placeholder `<instance>.<capability>`
/// references (rather than empty lists) so a freshly scaffolded project passes
/// `phoxal-cli validate`/`doctor` out of the box — robot validation rejects empty
/// actuator/encoder lists. The user replaces the placeholders with their real drive
/// instances, defined under `components.instances`.
pub fn robot_yaml(id: &str, runtime_train: &str) -> String {
    format!(
        r#"version: v1

identity:
  id: {id}
  namespace: dev

structure: structure.urdf

phoxal_runtimes:
  version: "{runtime_train}"

motion:
  kinematic:
    kind: differential
    # Placeholder drive references so a fresh scaffold validates. Replace these
    # with your robot's real `<instance>.<capability>` ids and define the
    # matching instances under `components.instances` below.
    left_actuators: [left_drive.motor]
    right_actuators: [right_drive.motor]
    left_encoders: [left_drive.encoder]
    right_encoders: [right_drive.encoder]
    wheel_radius_m: 0.1
    wheel_base_m: 0.5

components:
  sources: {{}}
  instances: {{}}
"#
    )
}

fn create_runtime(app: &AppContext, command: &Runtime) -> Result<()> {
    let robot_root = discovered_robot_root(app)?;
    let runtime_version = runtime_scaffold_phoxal_version(&robot_root)?;
    let runtime_dir = robot_root.join("runtimes").join(&command.name);
    fs::create_dir_all(runtime_dir.join("src"))
        .with_context(|| format!("failed to create {}", runtime_dir.display()))?;
    write_new(
        runtime_dir.join("Cargo.toml"),
        format!(
            r#"[package]
name = "{}"
version = "0.1.0"
edition = "2024"

[dependencies]
phoxal = "{}"
anyhow = "1.0.102"
async-trait = "0.1.89"
clap = {{ version = "4.6.1", features = ["derive", "env"] }}
tokio = {{ version = "1", features = ["macros", "rt", "rt-multi-thread", "signal"] }}
"#,
            command.name, runtime_version
        ),
    )?;
    write_new(
        runtime_dir.join("src/main.rs"),
        format!(
            r#"use anyhow::Result;
use phoxal::runtime::clock::Step;
use phoxal::runtime::{{Io, RobotRuntimeArgs, Runtime, RuntimeInputs}};

#[tokio::main]
async fn main() -> Result<()> {{
    phoxal::runtime::execute::<UserRuntime>().await
}}

#[derive(Debug, clap::Args)]
struct Args {{}}

struct UserRuntime;

#[async_trait::async_trait]
impl Runtime for UserRuntime {{
    const RUNTIME_ID: &'static str = "{}";

    type Args = Args;
    type Config = ();
    type Input = std::convert::Infallible;

    fn config(_args: &Self::Args, _common: &RobotRuntimeArgs) -> Result<Self::Config> {{
        Ok(())
    }}

    fn clock_period(_config: &Self::Config) -> std::time::Duration {{
        std::time::Duration::from_millis(20)
    }}

    async fn new(_io: &mut Io<Self::Input>, _config: Self::Config) -> Result<Self> {{
        Ok(Self)
    }}

    async fn step(&mut self, _step: Step, _inputs: RuntimeInputs<Self::Input>) -> Result<()> {{
        Ok(())
    }}
}}
"#,
            command.name
        ),
    )?;
    app.ui
        .success(format!("created runtime {}", runtime_dir.display()));
    Ok(())
}

fn create_component(app: &AppContext, command: &Component) -> Result<()> {
    let robot_root = discovered_robot_root(app)?;
    let component_dir = robot_root.join("components").join(&command.name);
    fs::create_dir_all(&component_dir)
        .with_context(|| format!("failed to create {}", component_dir.display()))?;
    write_component_files(&component_dir, &command.name, None)?;
    app.ui
        .success(format!("created component {}", component_dir.display()));
    Ok(())
}

fn create_catalog_component(app: &AppContext, command: &CatalogComponent) -> Result<()> {
    if command.gtin.len() != 13 || !command.gtin.chars().all(|ch| ch.is_ascii_digit()) {
        bail!("catalog component GTIN must be exactly 13 digits");
    }
    let dir = command
        .dir
        .clone()
        .unwrap_or_else(|| PathBuf::from(format!("component-{}", command.gtin)));
    let root = if dir.is_absolute() {
        dir
    } else {
        app.project.root().join(dir)
    };
    fs::create_dir_all(&root).with_context(|| format!("failed to create {}", root.display()))?;
    write_component_files(
        &root,
        &format!("component_{}", command.gtin),
        Some(&command.gtin),
    )?;
    app.ui
        .success(format!("created catalog component {}", root.display()));
    Ok(())
}

fn write_component_files(dir: &std::path::Path, name: &str, gtin: Option<&str>) -> Result<()> {
    let gtin_line = gtin.map_or(String::new(), |gtin| format!("gtin: \"{gtin}\"\n"));
    write_new(
        dir.join("component.yaml"),
        format!(
            r#"version: v1
{gtin_line}capabilities: {{}}
"#
        ),
    )?;
    write_new(
        dir.join("structure.urdf"),
        format!(
            r#"<?xml version="1.0"?>
<robot name="{name}">
  <link name="{name}_link"/>
</robot>
"#
        ),
    )?;
    write_new(
        dir.join("simulation.yaml"),
        "version: v1\nlinks: {}\ncapabilities: {}\n".to_string(),
    )?;
    Ok(())
}

fn discovered_robot_root(app: &AppContext) -> Result<PathBuf> {
    let robot_yaml = crate::resolver::discover_robot_yaml(app.project.root())?;
    robot_yaml
        .parent()
        .map(std::path::Path::to_path_buf)
        .context("robot.yaml did not have a parent directory")
}

fn runtime_scaffold_phoxal_version(robot_root: &std::path::Path) -> Result<String> {
    let lock_path = robot_root.join(LOCKFILE_NAME);
    if lock_path.is_file() {
        let lockfile = Lockfile::read(&lock_path)?;
        Ok(lockfile.phoxal_runtimes.resolved)
    } else {
        Ok(DEFAULT_RUNTIME_VERSION.to_string())
    }
}

fn write_new(path: PathBuf, contents: String) -> Result<()> {
    if path.exists() {
        bail!("refusing to overwrite {}", path.display());
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    fs::write(&path, contents).with_context(|| format!("failed to write {}", path.display()))
}
