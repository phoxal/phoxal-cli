use std::fs;

use anyhow::{Context, Result, bail};
use clap::Args;

use crate::AppContext;

#[derive(Debug, Args)]
pub struct Init {
    #[arg(long, default_value = "my-robot")]
    pub robot_id: String,
}

impl Init {
    pub async fn run(&self, app: &AppContext) -> Result<()> {
        let root = app.project.root();
        let robot = root.join("robot.yaml");
        if robot.exists() {
            bail!("{} already exists", robot.display());
        }
        let yaml = format!(
            "schema: robot/v0\nrobot:\n  id: {}\n  namespace: default\n  kinematic:\n    kind: differential\n    left_actuators: []\n    right_actuators: []\n    left_encoders: []\n    right_encoders: []\n    wheel_radius_m: 0.1\n    wheel_base_m: 0.5\n  components: {{}}\nartifacts:\n  channel: stable\n",
            self.robot_id
        );
        fs::write(&robot, yaml).with_context(|| format!("failed to write {}", robot.display()))?;

        let gitignore = root.join(".gitignore");
        let mut contents = fs::read_to_string(&gitignore).unwrap_or_default();
        if !contents.lines().any(|line| line.trim() == "/.phoxal/") {
            if !contents.is_empty() && !contents.ends_with('\n') {
                contents.push('\n');
            }
            contents.push_str("/.phoxal/\n");
            fs::write(&gitignore, contents)
                .with_context(|| format!("failed to write {}", gitignore.display()))?;
        }
        println!(
            "initialized {} and added /.phoxal/ to .gitignore",
            robot.display()
        );
        Ok(())
    }
}
