//! Pure process-launch values shared by project preparation and supervision.

use crate::identity::ProducerId;
use crate::session::launch_env;
use crate::session::{
    ParticipantInstanceKey, ParticipantKind, ParticipantLaunchCommand, ProcessKey, ReadinessPolicy,
    RobotKey, RuntimeFailurePolicy, StartupRequirement,
};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

pub const RESTART_DELAY: Duration = Duration::from_secs(2);
pub const START_LIMIT_INTERVAL: Duration = Duration::from_secs(60);
pub const START_LIMIT_BURST: usize = 5;

#[derive(Debug, Clone, PartialEq)]
pub struct ParticipantSpec {
    pub key: ProcessKey,
    pub id: String,
    pub kind: ParticipantKind,
    pub executable: PathBuf,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub env: Vec<(String, String)>,
    pub shutdown_grace: Duration,
    pub process_group: bool,
    pub note: Option<String>,
    pub bus_participant: bool,
    pub readiness: ReadinessPolicy,
    pub startup_requirement: StartupRequirement,
    pub runtime_failure: RuntimeFailurePolicy,
    pub restart_policy: RestartPolicy,
}

impl ParticipantSpec {
    #[must_use]
    pub fn exact_liveliness_template(robot: RobotKey, participant: &str) -> ReadinessPolicy {
        ReadinessPolicy::ExactLiveliness(ParticipantInstanceKey {
            robot,
            participant: participant.to_string(),
            producer: ProducerId::mint(),
        })
    }

    #[must_use]
    pub fn command_line(&self) -> String {
        let mut parts = vec![self.executable.display().to_string()];
        parts.extend(self.args.clone());
        parts.join(" ")
    }

    #[must_use]
    pub fn launch_command(&self) -> ParticipantLaunchCommand {
        ParticipantLaunchCommand {
            command_line: render_manual_command_line(self),
            env: self.env.iter().cloned().collect(),
        }
    }
}

#[must_use]
pub fn render_manual_command_line(spec: &ParticipantSpec) -> String {
    let env = spec.env.iter().cloned().collect::<BTreeMap<_, _>>();
    let mut parts = vec![shell_quote(&spec.executable.display().to_string())];
    parts.extend(spec.args.iter().map(|arg| shell_quote(arg)));
    for (env_key, flag) in launch_env::ENV_TO_FLAG {
        if let Some(value) = env.get(*env_key) {
            parts.push((*flag).to_string());
            parts.push(shell_quote(value));
        }
    }
    parts.join(" ")
}

#[must_use]
pub fn shell_quote(value: &str) -> String {
    if !value.is_empty()
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(byte, b'/' | b'.' | b'-' | b'_' | b':' | b',' | b'=' | b'@')
        })
    {
        return value.to_string();
    }
    let escaped = value.replace('\'', "'\\''");
    format!("'{escaped}'")
}

#[derive(Debug, Clone, PartialEq)]
pub struct RestartPolicy {
    pub restart_delay: Duration,
    pub start_limit_interval: Duration,
    pub start_limit_burst: usize,
}

impl Default for RestartPolicy {
    fn default() -> Self {
        Self {
            restart_delay: RESTART_DELAY,
            start_limit_interval: START_LIMIT_INTERVAL,
            start_limit_burst: START_LIMIT_BURST,
        }
    }
}
