use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};
use serde::Serialize;

use crate::AppContext;
use crate::commands::MessageFormat;

#[derive(Debug, Args)]
pub struct Robot {
    #[command(subcommand)]
    pub command: RobotSubcommand,
}

#[derive(Debug, Subcommand)]
pub enum RobotSubcommand {
    #[command(
        about = "Scaffold a new robot project.",
        long_about = "Scaffold a new robot project.\n\n\
                      Creates <name>/ with robot.yaml (root schema + api_version, default stable channel), structure.urdf, a default world, and a runtimes/ directory. Prints the v0 pre-stable warning."
    )]
    New(New),
}

#[derive(Debug, Args)]
pub struct New {
    #[arg(
        help = "Robot project name; must be kebab-case and becomes the default robot identity id."
    )]
    pub name: String,
    #[arg(
        long,
        value_enum,
        default_value_t = MessageFormat::Human,
        help = "Output format for the scaffold summary."
    )]
    pub message_format: MessageFormat,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NewRobotSummary {
    pub name: String,
    pub project_dir: PathBuf,
    pub robot_path: PathBuf,
    pub structure_path: PathBuf,
    pub world_path: PathBuf,
    pub api_version: String,
    pub channel: String,
}

impl Robot {
    pub async fn run(&self, app: &AppContext) -> Result<()> {
        match &self.command {
            RobotSubcommand::New(command) => command.run(app).await,
        }
    }
}

impl New {
    pub async fn run(&self, app: &AppContext) -> Result<()> {
        let root = app.project.root().to_path_buf();
        let name = self.name.clone();
        let summary = tokio::task::spawn_blocking(move || new_robot(&root, &name))
            .await
            .context("robot new worker failed")??;
        eprintln!(
            "warning: v0 is pre-stable: artifacts built at different times may not interoperate; pin digests with phoxal-cli deploy build"
        );
        crate::commands::print_message(
            &summary,
            || {
                println!("created robot project: {}", summary.project_dir.display());
                println!("wrote {}", summary.robot_path.display());
                println!("wrote {}", summary.structure_path.display());
                println!("wrote {}", summary.world_path.display());
                Ok(())
            },
            self.message_format,
        )
    }
}

pub fn new_robot(parent: &Path, name: &str) -> Result<NewRobotSummary> {
    let name = validate_robot_name(name)?;
    let project_dir = parent.join(name);
    if project_dir.exists() {
        bail!(
            "robot project directory already exists: {}",
            project_dir.display()
        );
    }
    fs::create_dir_all(project_dir.join("worlds"))
        .with_context(|| format!("failed to create {}", project_dir.join("worlds").display()))?;
    fs::create_dir_all(project_dir.join("runtimes")).with_context(|| {
        format!(
            "failed to create {}",
            project_dir.join("runtimes").display()
        )
    })?;

    let robot_path = project_dir.join("robot.yaml");
    let structure_path = project_dir.join("structure.urdf");
    let world_path = project_dir.join("worlds").join("default.wbt");
    fs::write(&robot_path, robot_yaml(name))
        .with_context(|| format!("failed to write {}", robot_path.display()))?;
    fs::write(&structure_path, structure_urdf(name))
        .with_context(|| format!("failed to write {}", structure_path.display()))?;
    fs::write(&world_path, default_world())
        .with_context(|| format!("failed to write {}", world_path.display()))?;

    Ok(NewRobotSummary {
        name: name.to_string(),
        project_dir,
        robot_path,
        structure_path,
        world_path,
        api_version: "y2026_1".to_string(),
        channel: "stable".to_string(),
    })
}

fn validate_robot_name(name: &str) -> Result<&str> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        bail!("robot name must not be empty");
    }
    if trimmed != name {
        bail!("robot name '{name}' must not contain leading or trailing whitespace");
    }
    if !is_valid_robot_name(name) {
        bail!(
            "robot name '{name}' must be kebab-case: start with a lowercase ASCII letter, then use lowercase letters, digits, and single hyphens"
        );
    }
    Ok(name)
}

fn is_valid_robot_name(name: &str) -> bool {
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

fn robot_yaml(name: &str) -> String {
    // The scaffold is deliberately self-consistent: every motion `CapabilityRef`
    // (left_drive.motor, right_drive.encoder, ...) resolves to a declared
    // component instance with the matching capability, so `phoxal-cli check`
    // reaches a meaningful graph check instead of failing on dangling motion
    // references. The instances are driverless placeholders (no `driver:` block),
    // so `check` does not try to build a non-existent driver crate; the user adds
    // a real component source + `driver:` block when wiring up hardware.
    format!(
        r#"schema: v0
api_version: y2026_1

identity:
  id: {name}
  namespace: dev

structure: structure.urdf

phoxal_participants:
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
  sources:
    # Replace this placeholder with the real component source (e.g. a git
    # catalog entry) and add a `driver:` block to each instance to wire up
    # hardware. See the docs for the full component schema.
    placeholder_wheel:
      path: components/placeholder_wheel
  instances:
    left_drive:
      component: placeholder_wheel
      mount_link: base_link
      parameters:
        motor:   {{ kind: motor,   direction_sign: 1 }}
        encoder: {{ kind: encoder, direction_sign: 1 }}
    right_drive:
      component: placeholder_wheel
      mount_link: base_link
      parameters:
        motor:   {{ kind: motor,   direction_sign: 1 }}
        encoder: {{ kind: encoder, direction_sign: 1 }}
"#
    )
}

fn structure_urdf(name: &str) -> String {
    format!(
        r#"<robot name="{name}">
  <link name="base_link"/>
</robot>
"#
    )
}

fn default_world() -> &'static str {
    "#VRML_SIM R2023b utf8\n\nWorldInfo {\n}\n"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn robot_new_scaffolds_project_layout() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let summary = new_robot(temp.path(), "rover-one")?;

        assert_eq!(summary.api_version, "y2026_1");
        assert!(summary.robot_path.is_file());
        assert!(summary.structure_path.is_file());
        assert!(summary.world_path.is_file());
        assert!(summary.project_dir.join("runtimes").is_dir());

        let robot = fs::read_to_string(summary.robot_path)?;
        assert!(robot.contains("schema: v0"));
        assert!(robot.contains("api_version: y2026_1"));
        assert!(robot.contains("channel: stable"));
        assert!(robot.contains("namespace: dev"));

        let error = new_robot(temp.path(), "rover-one")
            .expect_err("creating the same project twice should fail");
        assert!(error.to_string().contains("already exists"));

        Ok(())
    }

    #[test]
    fn robot_new_scaffold_is_self_consistent_and_reaches_a_meaningful_check() -> Result<()> {
        use phoxal::model::component::v1::CapabilityRef;
        use phoxal::model::robot::v1::{KinematicConfig, Robot};

        let temp = tempfile::tempdir()?;
        let summary = new_robot(temp.path(), "rover-one")?;
        let yaml = fs::read_to_string(&summary.robot_path)?;

        // 1. The generated manifest parses and passes model validation.
        let robot = Robot::parse_from_string(&yaml).expect("scaffold robot.yaml should parse");
        robot
            .validate()
            .expect("scaffold robot.yaml should pass model validation");

        // 2. Every motion CapabilityRef resolves to a declared component instance
        //    with the matching capability. This is what makes `phoxal-cli check`
        //    reach a meaningful topology check: with the round-1 fix, a dangling
        //    motion ref would otherwise surface as an UnresolvedComponentTemplate.
        let KinematicConfig::Differential {
            left_actuators,
            right_actuators,
            left_encoders,
            right_encoders,
            ..
        } = &robot.motion.kinematic
        else {
            panic!("scaffold should use a differential kinematic config");
        };
        let resolves = |capability: &CapabilityRef| {
            robot
                .components
                .instances
                .get(&capability.component_id)
                .is_some_and(|instance| instance.parameters.contains_key(&capability.capability_id))
        };
        for capability in left_actuators
            .iter()
            .chain(right_actuators)
            .chain(left_encoders)
            .chain(right_encoders)
        {
            assert!(
                resolves(capability),
                "motion ref {capability} does not bind to a declared component capability"
            );
        }

        // 3. The placeholder instances are driverless, so `check` never tries to
        //    build a non-existent driver crate from the scaffold.
        assert!(
            robot
                .components
                .instances
                .values()
                .all(|instance| instance.driver.is_none()),
            "scaffold component instances should be driverless placeholders"
        );

        Ok(())
    }
}
