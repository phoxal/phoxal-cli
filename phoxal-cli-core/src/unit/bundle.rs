use crate::AppContext;
use crate::unit::robot::ValidatedRobot;
use derive_new::new;
use derive_setters::Setters;
use std::path::PathBuf;

#[derive(Debug, new, Setters)]
#[setters(prefix = "with_", strip_option, into)]
pub struct Bundle {
    #[new(into)]
    #[setters(skip)]
    robot_model: String,

    /// Optional, custom directory to bundle robot.
    #[new(default)]
    output: Option<PathBuf>,
}

impl super::Unit for Bundle {
    type Output = phoxal_engine::staged::Robot;

    fn name(&self) -> &'static str {
        "Bundle"
    }

    fn execute(&self, app: &AppContext) -> anyhow::Result<Self::Output> {
        let robot = ValidatedRobot::load(app, &self.robot_model)?;
        robot.stage_bundle(app, self.output.as_deref())
    }
}
