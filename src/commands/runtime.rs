use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};
use heck::ToUpperCamelCase;
use phoxal::model::robot::v1::UserRuntime;
use semver::Version;
use toml::Value as TomlValue;

use crate::AppContext;
use crate::resolver::{discover_robot_yaml, load_robot};

#[derive(Debug, Args)]
pub struct Runtime {
    #[command(subcommand)]
    pub command: RuntimeSubcommand,
}

#[derive(Debug, Subcommand)]
pub enum RuntimeSubcommand {
    #[command(about = "Scaffold a user runtime crate and register it in robot.yaml.")]
    Add(Add),
}

#[derive(Debug, Args)]
pub struct Add {
    #[arg(help = "Runtime id, used as the crate name and user_runtimes key.")]
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddRuntimeOutcome {
    pub name: String,
    pub api_version: String,
    pub crate_dir: PathBuf,
    pub manifest_path: PathBuf,
}

impl Runtime {
    pub async fn run(&self, app: &AppContext) -> Result<()> {
        match &self.command {
            RuntimeSubcommand::Add(command) => command.run(app).await,
        }
    }
}

impl Add {
    pub async fn run(&self, app: &AppContext) -> Result<()> {
        let outcome = add_runtime(app.project.root(), &self.name)?;
        println!("created runtime crate: {}", outcome.crate_dir.display());
        println!(
            "registered manifest entry: user_runtimes.{} = {{ path: \"{}\" }}",
            outcome.name,
            outcome.manifest_path.display()
        );
        println!(
            "next: run `phoxal check`; later `phoxal runtime run {}`",
            outcome.name
        );
        Ok(())
    }
}

pub fn add_runtime(project_start: &Path, name: &str) -> Result<AddRuntimeOutcome> {
    let name = validate_runtime_name(name)?;
    let robot_path = discover_robot_yaml(project_start)
        .with_context(|| format!("failed to find robot.yaml from {}", project_start.display()))?;
    let project_root = robot_path
        .parent()
        .context("robot.yaml did not have a parent directory")?;
    let mut robot = load_robot(&robot_path)?;
    let api_version = robot.api_version.clone();
    let manifest_path = PathBuf::from("runtimes").join(name);
    let crate_dir = project_root.join(&manifest_path);

    if crate_dir.exists() {
        bail!(
            "runtime crate directory already exists: {}",
            crate_dir.display()
        );
    }
    if robot.user_runtimes.contains_key(name) {
        bail!(
            "user_runtimes.{name} already exists in {}",
            robot_path.display()
        );
    }

    let phoxal_version = cli_phoxal_dependency_major_minor()?;
    scaffold_runtime_crate(&crate_dir, name, &api_version, &phoxal_version)?;
    robot.user_runtimes.insert(
        name.to_string(),
        UserRuntime {
            path: manifest_path.clone(),
            framework: "match-platform".to_string(),
            build: None,
        },
    );
    robot
        .write_to_dir(project_root)
        .with_context(|| format!("failed to update {}", robot_path.display()))?;

    Ok(AddRuntimeOutcome {
        name: name.to_string(),
        api_version,
        crate_dir,
        manifest_path,
    })
}

fn validate_runtime_name(name: &str) -> Result<&str> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        bail!("runtime name must not be empty");
    }
    if trimmed != name {
        bail!("runtime name '{name}' must not contain leading or trailing whitespace");
    }
    if !is_valid_runtime_name(name) {
        bail!(
            "runtime name '{name}' must be kebab-case: start with a lowercase ASCII letter, then use lowercase letters, digits, and single hyphens"
        );
    }
    Ok(name)
}

fn is_valid_runtime_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    let Some((&first, rest)) = bytes.split_first() else {
        return false;
    };
    if !first.is_ascii_lowercase() {
        return false;
    }

    let mut previous_was_hyphen = false;
    for &byte in rest {
        if byte.is_ascii_lowercase() || byte.is_ascii_digit() {
            previous_was_hyphen = false;
        } else if byte == b'-' {
            if previous_was_hyphen {
                return false;
            }
            previous_was_hyphen = true;
        } else {
            return false;
        }
    }

    !previous_was_hyphen
}

fn scaffold_runtime_crate(
    crate_dir: &Path,
    name: &str,
    api_version: &str,
    phoxal_version: &str,
) -> Result<()> {
    let src_dir = crate_dir.join("src");
    fs::create_dir_all(&src_dir)
        .with_context(|| format!("failed to create {}", src_dir.display()))?;
    fs::write(
        crate_dir.join("Cargo.toml"),
        runtime_cargo_toml(name, phoxal_version),
    )
    .with_context(|| format!("failed to write {}", crate_dir.join("Cargo.toml").display()))?;
    fs::write(
        src_dir.join("main.rs"),
        runtime_main_rs(name, api_version, &name.to_upper_camel_case()),
    )
    .with_context(|| format!("failed to write {}", src_dir.join("main.rs").display()))?;
    Ok(())
}

fn runtime_cargo_toml(name: &str, phoxal_version: &str) -> String {
    format!(
        r#"[package]
name = "{name}"
version = "0.1.0"
edition = "2024"

[dependencies]
phoxal = "{phoxal_version}"
anyhow = "1"
serde = {{ version = "1", features = ["derive"] }}

[[bin]]
name = "{name}"
path = "src/main.rs"
"#
    )
}

fn runtime_main_rs(name: &str, api_version: &str, pascal_name: &str) -> String {
    format!(
        r#"// `api` is your one API version (D59). Remove this `allow` once you use it
// in a handle field below (e.g. `Publisher<api::drive::Target>`).
#[allow(unused_imports)]
use phoxal::api::{api_version} as api;
use phoxal::prelude::*;

/// Typed config for this runtime (validated by `phoxal check`).
#[derive(Debug, serde::Deserialize)]
pub struct Config {{
    // Add config fields here.
}}

#[derive(phoxal::Runtime)]
#[phoxal(id = "{name}", api = {api_version}, config = Config)]
struct {pascal_name} {{
    // TODO: declare typed handle fields and use Config in setup when needed.
    // target: Publisher<api::drive::Target>,
}}

#[phoxal::runtime]
impl {pascal_name} {{
    #[setup]
    async fn setup(_ctx: &mut SetupContext<Self>) -> Result<Self> {{
        Ok(Self {{}})
    }}

    #[step(hz = 10)]
    async fn step(&mut self, _step: StepContext) -> Result<()> {{
        Ok(())
    }}
}}

fn main() -> phoxal::Result<()> {{
    phoxal::run::<{pascal_name}>()
}}
"#
    )
}

fn cli_phoxal_dependency_major_minor() -> Result<String> {
    let manifest_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
    let contents = fs::read_to_string(&manifest_path)
        .with_context(|| format!("failed to read {}", manifest_path.display()))?;
    let manifest = toml::from_str::<TomlValue>(&contents)
        .with_context(|| format!("failed to parse {}", manifest_path.display()))?;
    let version = phoxal_dependency_version(&manifest).with_context(|| {
        format!(
            "failed to find phoxal dependency version in {}",
            manifest_path.display()
        )
    })?;
    let version = parse_dependency_version(&version)?;
    Ok(format!("{}.{}", version.major, version.minor))
}

fn phoxal_dependency_version(manifest: &TomlValue) -> Option<String> {
    let dependency = manifest
        .get("dependencies")
        .and_then(|dependencies| dependencies.get("phoxal"))?;
    if let Some(version) = dependency_version(dependency) {
        return Some(version);
    }
    if !is_workspace_dependency(dependency) {
        return None;
    }
    manifest
        .get("workspace")
        .and_then(|workspace| workspace.get("dependencies"))
        .and_then(|dependencies| dependencies.get("phoxal"))
        .and_then(dependency_version)
}

fn dependency_version(dependency: &TomlValue) -> Option<String> {
    if let Some(version) = dependency.as_str() {
        return Some(version.to_string());
    }
    dependency
        .as_table()
        .and_then(|table| table.get("version"))
        .and_then(TomlValue::as_str)
        .map(str::to_string)
}

fn is_workspace_dependency(dependency: &TomlValue) -> bool {
    dependency
        .as_table()
        .and_then(|table| table.get("workspace"))
        .and_then(TomlValue::as_bool)
        .unwrap_or(false)
}

fn parse_dependency_version(version: &str) -> Result<Version> {
    let trimmed = version
        .trim()
        .trim_start_matches('^')
        .trim_start_matches('~')
        .trim_start_matches('=')
        .trim_start_matches('v');
    Version::parse(trimmed)
        .or_else(|_| Version::parse(&format!("{trimmed}.0")))
        .with_context(|| format!("phoxal dependency version '{version}' is not a semver version"))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use phoxal::model::robot::RobotV1 as Robot;

    use super::*;

    #[test]
    fn add_runtime_scaffolds_crate_and_registers_manifest() -> Result<()> {
        let temp = tempfile::tempdir()?;
        fs::write(temp.path().join("robot.yaml"), minimal_robot_yaml())?;

        let outcome = add_runtime(temp.path(), "avoid-obstacles")?;
        assert_eq!(outcome.name, "avoid-obstacles");
        assert_eq!(outcome.api_version, "y2026_1");
        assert_eq!(
            outcome.crate_dir,
            temp.path().join("runtimes").join("avoid-obstacles")
        );

        let cargo_toml = temp
            .path()
            .join("runtimes")
            .join("avoid-obstacles")
            .join("Cargo.toml");
        let main_rs = temp
            .path()
            .join("runtimes")
            .join("avoid-obstacles")
            .join("src")
            .join("main.rs");
        assert!(cargo_toml.is_file());
        assert!(main_rs.is_file());

        let main = fs::read_to_string(main_rs)?;
        assert!(main.contains(r#"#[phoxal(id = "avoid-obstacles", api = y2026_1"#));
        assert!(main.contains("phoxal::run::<AvoidObstacles>()"));

        let robot = Robot::read_from_dir(temp.path())?;
        let runtime = robot
            .user_runtimes
            .get("avoid-obstacles")
            .expect("runtime should be registered");
        assert_eq!(
            runtime.path,
            PathBuf::from("runtimes").join("avoid-obstacles")
        );

        let error = add_runtime(temp.path(), "avoid-obstacles")
            .expect_err("adding the same runtime twice should fail");
        assert!(error.to_string().contains("already exists"));

        Ok(())
    }

    #[test]
    fn invalid_runtime_names_are_rejected() {
        for name in [
            "",
            "AvoidObstacles",
            "avoid_obstacles",
            "-avoid",
            "avoid-",
            "avoid--it",
        ] {
            assert!(
                validate_runtime_name(name).is_err(),
                "{name:?} should be rejected"
            );
        }
        assert_eq!(
            validate_runtime_name("avoid-obstacles").unwrap(),
            "avoid-obstacles"
        );
    }

    fn minimal_robot_yaml() -> &'static str {
        r#"schema: v0
api_version: y2026_1

identity:
  id: testbot
  namespace: test

structure: structure.urdf

phoxal_runtimes:
  channel: stable

motion:
  kinematic:
    kind: differential
    left_actuators: [left_drive.motor]
    right_actuators: [right_drive.motor]
    left_encoders: [left_drive.encoder]
    right_encoders: [right_drive.encoder]
    wheel_radius_m: 0.1
    wheel_base_m: 0.5

components:
  sources: {}
  instances: {}
"#
    }
}
