use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use clap::{Args, Subcommand};
use heck::ToUpperCamelCase;
use semver::Version;
use serde::Serialize;
use serde_yaml::{Mapping as YamlMapping, Value as YamlValue};
use toml::Value as TomlValue;

use crate::AppContext;
use crate::commands::MessageFormat;
use crate::resolver::{discover_robot_yaml, load_robot_with_extras, target_generation_for_robot};

#[derive(Debug, Args)]
pub struct Service {
    #[command(subcommand)]
    pub command: ServiceSubcommand,
}

#[derive(Debug, Subcommand)]
pub enum ServiceSubcommand {
    #[command(
        about = "Scaffold a user service crate and register it in robot.yaml.",
        long_about = "Scaffold a user service crate and register it in robot.yaml.\n\n\
                      Generates runtimes/<name>/ with a #[derive(phoxal::Service)] crate (mandatory #[setup], a #[step], and a blocking phoxal::run) and adds a user_participants.<name> entry to robot.yaml."
    )]
    Add(Add),
    #[command(about = "Print official services from the configured artifact catalog.")]
    Catalog(Catalog),
}

#[derive(Debug, Args)]
pub struct Add {
    #[arg(help = "Service id; use [a-z0-9_], used as the crate name and user_participants key.")]
    pub name: String,
}

#[derive(Debug, Args)]
pub struct Catalog {
    #[arg(
        long,
        value_enum,
        default_value_t = MessageFormat::Human,
        help = "Output format for the catalog listing."
    )]
    pub message_format: MessageFormat,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddServiceOutcome {
    pub name: String,
    pub target_generation: String,
    pub crate_dir: PathBuf,
    pub manifest_path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ServiceCatalogSummary {
    pub entries: Vec<ServiceCatalogEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ServiceCatalogEntry {
    pub id: String,
    pub api_generations: Vec<String>,
    pub participant_kind: &'static str,
}

impl Service {
    pub async fn run(&self, app: &AppContext) -> Result<()> {
        match &self.command {
            ServiceSubcommand::Add(command) => command.run(app).await,
            ServiceSubcommand::Catalog(command) => command.run(app).await,
        }
    }
}

impl Add {
    pub async fn run(&self, app: &AppContext) -> Result<()> {
        let outcome = add_service(app.project.root(), &self.name, app.catalog_source.clone())?;
        println!("created service crate: {}", outcome.crate_dir.display());
        println!(
            "registered manifest entry: user_participants.{} = {{ path: \"{}\" }}",
            outcome.name,
            outcome.manifest_path.display()
        );
        println!(
            "next: run `phoxal-cli check`; later `phoxal-cli run --watch` to hot-swap {} inside the live graph",
            outcome.name
        );
        Ok(())
    }
}

impl Catalog {
    pub async fn run(&self, app: &AppContext) -> Result<()> {
        let summary = service_catalog_summary(app.project.root(), app.catalog_source.clone())?;
        crate::commands::print_message(
            &summary,
            || {
                for entry in &summary.entries {
                    println!(
                        "{} -> api_generations [{}] ({})",
                        entry.id,
                        entry.api_generations.join(", "),
                        entry.participant_kind
                    );
                }
                Ok(())
            },
            self.message_format,
        )
    }
}

pub fn service_catalog_summary(
    project_start: &Path,
    catalog_source: Option<String>,
) -> Result<ServiceCatalogSummary> {
    let robot_path = discover_robot_yaml(project_start)
        .with_context(|| format!("failed to find robot.yaml from {}", project_start.display()))?;
    let project_root = robot_path
        .parent()
        .context("robot.yaml did not have a parent directory")?;
    let loaded = load_robot_with_extras(&robot_path)?;
    let catalog = crate::commands::load_catalog_for_robot_from_source(
        catalog_source,
        project_root,
        &loaded.extras,
        false,
    )?
    .ok_or_else(crate::catalog::unavailable_catalog_error)?;
    Ok(ServiceCatalogSummary {
        entries: catalog
            .entries
            .iter()
            .filter(|entry| entry.kind == crate::catalog::ArtifactKind::Service)
            .map(|entry| ServiceCatalogEntry {
                id: entry.artifact_id.clone(),
                api_generations: vec![entry.api_generation.clone()],
                participant_kind: "service",
            })
            .collect(),
    })
}

pub fn add_service(
    project_start: &Path,
    name: &str,
    catalog_source: Option<String>,
) -> Result<AddServiceOutcome> {
    let name = validate_service_name(name)?;
    let robot_path = discover_robot_yaml(project_start)
        .with_context(|| format!("failed to find robot.yaml from {}", project_start.display()))?;
    let project_root = robot_path
        .parent()
        .context("robot.yaml did not have a parent directory")?;
    let mut robot_yaml = read_robot_yaml(&robot_path)?;
    let loaded = load_robot_with_extras(&robot_path)?;
    let robot = loaded.robot;
    let catalog = crate::commands::load_catalog_for_robot_from_source(
        catalog_source,
        project_root,
        &loaded.extras,
        false,
    )?;
    let target_generation = target_generation_for_robot(&robot, catalog.as_ref())?;
    let manifest_path = PathBuf::from("runtimes").join(name);
    let crate_dir = project_root.join(&manifest_path);

    if crate_dir.exists() {
        bail!(
            "service crate directory already exists: {}",
            crate_dir.display()
        );
    }
    if robot.user_participants.contains_key(name) {
        bail!(
            "user_participants.{name} already exists in {}",
            robot_path.display()
        );
    }

    let phoxal_version = cli_phoxal_dependency_major_minor()?;
    let phoxal_api_version = cli_phoxal_api_scaffold_version()?;
    scaffold_service_crate(
        &crate_dir,
        name,
        &target_generation,
        &phoxal_version,
        &phoxal_api_version,
    )?;
    insert_user_service_manifest_entry(&mut robot_yaml, name, &manifest_path)?;
    write_robot_yaml(&robot_path, &robot_yaml)?;

    Ok(AddServiceOutcome {
        name: name.to_string(),
        target_generation,
        crate_dir,
        manifest_path,
    })
}

fn read_robot_yaml(path: &Path) -> Result<YamlValue> {
    let contents = fs::read_to_string(path)
        .with_context(|| format!("failed to read robot file {}", path.display()))?;
    serde_yaml::from_str(&contents)
        .with_context(|| format!("failed to parse robot file {}", path.display()))
}

fn write_robot_yaml(path: &Path, yaml: &YamlValue) -> Result<()> {
    let contents = serde_yaml::to_string(yaml)
        .with_context(|| format!("failed to serialize {}", path.display()))?;
    fs::write(path, contents).with_context(|| format!("failed to update {}", path.display()))
}

fn insert_user_service_manifest_entry(
    yaml: &mut YamlValue,
    name: &str,
    manifest_path: &Path,
) -> Result<()> {
    let root = yaml
        .as_mapping_mut()
        .ok_or_else(|| anyhow!("robot.yaml root must be a mapping"))?;
    let user_participants_key = YamlValue::String("user_participants".to_string());
    if root.get(&user_participants_key).is_none() {
        root.insert(
            user_participants_key.clone(),
            YamlValue::Mapping(YamlMapping::new()),
        );
    }
    let user_participants = root
        .get_mut(&user_participants_key)
        .and_then(YamlValue::as_mapping_mut)
        .ok_or_else(|| anyhow!("robot.yaml user_participants must be a mapping"))?;
    let service_key = YamlValue::String(name.to_string());
    if user_participants.contains_key(&service_key) {
        bail!("user_participants.{name} already exists in robot.yaml");
    }

    let mut service = YamlMapping::new();
    service.insert(
        YamlValue::String("path".to_string()),
        YamlValue::String(manifest_path_string(manifest_path)),
    );
    service.insert(
        YamlValue::String("framework".to_string()),
        YamlValue::String("match-platform".to_string()),
    );
    user_participants.insert(service_key, YamlValue::Mapping(service));
    Ok(())
}

fn manifest_path_string(path: &Path) -> String {
    path.iter()
        .map(|segment| segment.to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

fn validate_service_name(name: &str) -> Result<&str> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        bail!("service name must not be empty");
    }
    if trimmed != name {
        bail!("service name '{name}' must not contain leading or trailing whitespace");
    }
    if !is_valid_service_name(name) {
        bail!(
            "service name '{name}' must use only lowercase ASCII letters, digits, and underscores; '-' is reserved as a launch separator"
        );
    }
    Ok(name)
}

fn is_valid_service_name(name: &str) -> bool {
    crate::resolver::is_launch_id(name)
}

fn scaffold_service_crate(
    crate_dir: &Path,
    name: &str,
    api_version: &str,
    phoxal_version: &str,
    phoxal_api_version: &str,
) -> Result<()> {
    let src_dir = crate_dir.join("src");
    fs::create_dir_all(&src_dir)
        .with_context(|| format!("failed to create {}", src_dir.display()))?;
    fs::write(
        crate_dir.join("Cargo.toml"),
        service_cargo_toml(name, phoxal_version, phoxal_api_version),
    )
    .with_context(|| format!("failed to write {}", crate_dir.join("Cargo.toml").display()))?;
    fs::write(
        src_dir.join("main.rs"),
        service_main_rs(name, api_version, &name.to_upper_camel_case()),
    )
    .with_context(|| format!("failed to write {}", src_dir.join("main.rs").display()))?;
    Ok(())
}

fn service_cargo_toml(name: &str, phoxal_version: &str, phoxal_api_version: &str) -> String {
    format!(
        r#"[package]
name = "{name}"
version = "0.1.0"
edition = "2024"

[dependencies]
phoxal = "{phoxal_version}"
phoxal-api = "{phoxal_api_version}"
anyhow = "1"
serde = {{ version = "1", features = ["derive"] }}

[[bin]]
name = "{name}"
path = "src/main.rs"
"#
    )
}

fn service_main_rs(name: &str, api_version: &str, pascal_name: &str) -> String {
    format!(
        r#"use phoxal::prelude::*;
// `api` is your one API version (D59). Remove this `allow` once you use it
// in a handle field below (e.g. `Publisher<api::drive::Target>`).
#[allow(unused_imports)]
use phoxal_api::{api_version} as api;

#[derive(phoxal::Service)]
#[phoxal(id = "{name}", api = {api_version})]
struct {pascal_name} {{
    // TODO: declare typed handle fields here.
    // target: Publisher<api::drive::Target>,
}}

#[phoxal::behavior]
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
    let (manifest_path, manifest) = read_cli_manifest()?;
    let version = phoxal_dependency_version(&manifest).with_context(|| {
        format!(
            "failed to find phoxal dependency version in {}",
            manifest_path.display()
        )
    })?;
    let version = parse_dependency_version("phoxal dependency", &version)?;
    Ok(format!("{}.{}", version.major, version.minor))
}

fn cli_phoxal_api_scaffold_version() -> Result<String> {
    let (manifest_path, manifest) = read_cli_manifest()?;
    let version = phoxal_api_scaffold_version(&manifest).with_context(|| {
        format!(
            "invalid package.metadata.scaffold.phoxal-api in {}",
            manifest_path.display()
        )
    })?;
    let version = parse_dependency_version("package.metadata.scaffold.phoxal-api", &version)?;
    Ok(format!("{}.{}", version.major, version.minor))
}

fn read_cli_manifest() -> Result<(PathBuf, TomlValue)> {
    let manifest_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
    let contents = fs::read_to_string(&manifest_path)
        .with_context(|| format!("failed to read {}", manifest_path.display()))?;
    let manifest = toml::from_str::<TomlValue>(&contents)
        .with_context(|| format!("failed to parse {}", manifest_path.display()))?;
    Ok((manifest_path, manifest))
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
    let table = dependency.as_table()?;
    if let Some(version) = table.get("version").and_then(TomlValue::as_str) {
        return Some(version.to_string());
    }
    table
        .get("path")
        .and_then(TomlValue::as_str)
        .and_then(path_dependency_package_version)
}

fn path_dependency_package_version(path: &str) -> Option<String> {
    let path = Path::new(path);
    let dependency_root = if path.is_absolute() {
        path.to_path_buf()
    } else {
        Path::new(env!("CARGO_MANIFEST_DIR")).join(path)
    };
    let manifest = fs::read_to_string(dependency_root.join("Cargo.toml")).ok()?;
    let manifest = toml::from_str::<TomlValue>(&manifest).ok()?;
    manifest
        .get("package")
        .and_then(|package| package.get("version"))
        .and_then(TomlValue::as_str)
        .map(str::to_string)
}

fn phoxal_api_scaffold_version(manifest: &TomlValue) -> Result<String> {
    let value = manifest
        .get("package")
        .and_then(|package| package.get("metadata"))
        .and_then(|metadata| metadata.get("scaffold"))
        .and_then(|scaffold| scaffold.get("phoxal-api"))
        .context("missing package.metadata.scaffold.phoxal-api")?;
    value
        .as_str()
        .map(str::to_string)
        .context("package.metadata.scaffold.phoxal-api must be a string")
}

fn is_workspace_dependency(dependency: &TomlValue) -> bool {
    dependency
        .as_table()
        .and_then(|table| table.get("workspace"))
        .and_then(TomlValue::as_bool)
        .unwrap_or(false)
}

fn parse_dependency_version(label: &str, version: &str) -> Result<Version> {
    let trimmed = version
        .trim()
        .trim_start_matches('^')
        .trim_start_matches('~')
        .trim_start_matches('=')
        .trim_start_matches('v');
    Version::parse(trimmed)
        .or_else(|_| Version::parse(&format!("{trimmed}.0")))
        .with_context(|| format!("{label} version '{version}' is not a semver version"))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use phoxal::model::robot::RobotV1 as Robot;

    use super::*;

    #[test]
    fn add_service_scaffolds_crate_and_registers_manifest() -> Result<()> {
        let temp = tempfile::tempdir()?;
        fs::write(temp.path().join("robot.yaml"), minimal_robot_yaml())?;

        let outcome = add_service(temp.path(), "avoid_obstacles", None)?;
        assert_eq!(outcome.name, "avoid_obstacles");
        assert_eq!(outcome.target_generation, "y2026_1");
        assert_eq!(
            outcome.crate_dir,
            temp.path().join("runtimes").join("avoid_obstacles")
        );

        let cargo_toml = temp
            .path()
            .join("runtimes")
            .join("avoid_obstacles")
            .join("Cargo.toml");
        let main_rs = temp
            .path()
            .join("runtimes")
            .join("avoid_obstacles")
            .join("src")
            .join("main.rs");
        assert!(cargo_toml.is_file());
        assert!(main_rs.is_file());

        let main = fs::read_to_string(main_rs)?;
        assert!(main.contains(r#"#[phoxal(id = "avoid_obstacles", api = y2026_1"#));
        assert!(main.contains("#[derive(phoxal::Service)]"));
        assert!(main.contains("#[phoxal::behavior]"));
        assert!(main.contains("async fn setup(_ctx: &mut SetupContext<Self>) -> Result<Self>"));
        assert!(main.contains("phoxal::run::<AvoidObstacles>()"));
        assert!(!main.contains("phoxal::Runtime"));
        assert!(!main.contains("#[phoxal::runtime]"));
        // Scaffolds author against the published api crate (DoD #313): the API
        // version comes through `phoxal_api::y2026_1`, not the removed
        // `phoxal::api` facade.
        assert!(main.contains("use phoxal_api::y2026_1 as api;"));
        assert!(!main.contains("phoxal::api"));

        // The generated manifest depends on both `phoxal` and the api crate it
        // imports, pinned to their independent major.minor release lines.
        let cargo = fs::read_to_string(&cargo_toml)?;
        let phoxal_version = cli_phoxal_dependency_major_minor()?;
        let phoxal_api_version = cli_phoxal_api_scaffold_version()?;
        assert!(cargo.contains(&format!("phoxal = \"{phoxal_version}\"")));
        assert!(cargo.contains(&format!("phoxal-api = \"{phoxal_api_version}\"")));

        let robot = Robot::read_from_dir(temp.path())?;
        let service = robot
            .user_participants
            .get("avoid_obstacles")
            .expect("service should be registered");
        assert_eq!(
            service.path,
            PathBuf::from("runtimes").join("avoid_obstacles")
        );

        let error = add_service(temp.path(), "avoid_obstacles", None)
            .expect_err("adding the same service twice should fail");
        assert!(error.to_string().contains("already exists"));

        Ok(())
    }

    #[test]
    fn add_service_preserves_other_user_participant_config_extras() -> Result<()> {
        let temp = tempfile::tempdir()?;
        fs::write(
            temp.path().join("robot.yaml"),
            minimal_robot_yaml_with_user_participant(
                "brain",
                r#"
    path: runtimes/brain
    config:
      planner:
        gain: 0.7
        labels: [left, right]
"#,
            ),
        )?;

        add_service(temp.path(), "avoid_obstacles", None)?;

        let robot_yaml = fs::read_to_string(temp.path().join("robot.yaml"))?;
        let yaml: YamlValue = serde_yaml::from_str(&robot_yaml)?;
        let user_participants = yaml
            .get("user_participants")
            .and_then(YamlValue::as_mapping)
            .expect("user_participants should remain a mapping");
        let brain = user_participants
            .get(YamlValue::String("brain".to_string()))
            .and_then(YamlValue::as_mapping)
            .expect("existing user participant should survive");
        let labels = brain
            .get(YamlValue::String("config".to_string()))
            .and_then(|config| config.get("planner"))
            .and_then(|planner| planner.get("labels"))
            .and_then(YamlValue::as_sequence)
            .expect("nested config labels should survive");
        assert_eq!(
            labels,
            &vec![
                YamlValue::String("left".to_string()),
                YamlValue::String("right".to_string())
            ]
        );

        let avoid = user_participants
            .get(YamlValue::String("avoid_obstacles".to_string()))
            .and_then(YamlValue::as_mapping)
            .expect("new service should be registered");
        assert_eq!(
            avoid.get(YamlValue::String("path".to_string())),
            Some(&YamlValue::String("runtimes/avoid_obstacles".to_string()))
        );

        let loaded = load_robot_with_extras(&temp.path().join("robot.yaml"))?;
        assert_eq!(
            loaded.extras.user_runtime_config("brain"),
            Some(&serde_json::json!({
                "planner": {
                    "gain": 0.7,
                    "labels": ["left", "right"],
                }
            }))
        );
        assert!(
            loaded
                .robot
                .user_participants
                .contains_key("avoid_obstacles")
        );

        Ok(())
    }

    #[test]
    fn invalid_service_names_are_rejected() {
        for name in [
            "",
            "AvoidObstacles",
            "avoid-obstacles",
            "-avoid",
            "avoid-",
            "avoid--it",
        ] {
            assert!(
                validate_service_name(name).is_err(),
                "{name:?} should be rejected"
            );
        }
        assert_eq!(
            validate_service_name("avoid_obstacles").unwrap(),
            "avoid_obstacles"
        );
    }

    fn minimal_robot_yaml() -> &'static str {
        r#"schema: v0
api_version: y2026_1

identity:
  id: testbot
  namespace: test

structure: structure.urdf

phoxal_artifacts:
  channel: stable
phoxal_participants: {}

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

    fn minimal_robot_yaml_with_user_participant(name: &str, participant_yaml: &str) -> String {
        minimal_robot_yaml().replace(
            "\ncomponents:\n",
            &format!("\nuser_participants:\n  {name}:{participant_yaml}\ncomponents:\n"),
        )
    }
}
