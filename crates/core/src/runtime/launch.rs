//! Pure process-launch values shared by project preparation and supervision.

use crate::identity::ProducerId;
use crate::runtime::{
    ParticipantInstanceKey, ParticipantKind, ProcessKey, ReadinessPolicy, RobotKey,
    RuntimeFailurePolicy, StartupRequirement,
};
use anyhow::{Context, Result, bail};
use phoxal_runtime_contract::{ParticipantLaunch, env};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

pub const RESTART_DELAY: Duration = Duration::from_secs(2);
pub const START_LIMIT_INTERVAL: Duration = Duration::from_secs(60);
pub const START_LIMIT_BURST: usize = 5;
pub const MAX_CONFIG_ENV_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParticipantLaunchCommand {
    pub command_line: String,
    pub env: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedParticipantEnv {
    variables: BTreeMap<String, String>,
}

impl EncodedParticipantEnv {
    #[must_use]
    pub fn variables(&self) -> &BTreeMap<String, String> {
        &self.variables
    }

    #[must_use]
    pub fn spawn_env(&self) -> Vec<(String, String)> {
        self.variables
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect()
    }
}

pub const ENV_TO_FLAG: &[(&str, &str)] = &[
    (env::PARTICIPANT_ID, "--participant-id"),
    (env::ROBOT_ID, "--robot-id"),
    (env::NAMESPACE, "--namespace"),
    (env::BUNDLE_ROOT, "--bundle-root"),
    (env::COMPONENT_INSTANCE, "--component-instance"),
    (env::EXECUTION_ID, "--execution-id"),
    (env::PRODUCER_ID, "--producer-id"),
    (env::EXECUTION_ORIGIN, "--execution-origin"),
    (env::CONNECT, "--connect"),
    (env::CONFIG, "--config"),
    (env::CLOCK, "--clock"),
];

pub fn encode_participant_env(launch: &ParticipantLaunch) -> Result<EncodedParticipantEnv> {
    let variables = launch
        .encode_env()
        .with_context(|| {
            format!(
                "failed to encode launch environment for participant {}",
                launch.participant_id
            )
        })?
        .into_iter()
        .map(|(key, value)| (key.to_string(), value))
        .collect::<BTreeMap<_, _>>();
    validate_config_size(launch, &variables)?;
    Ok(EncodedParticipantEnv { variables })
}

pub fn encode_tool_env(launch: &ParticipantLaunch) -> Result<EncodedParticipantEnv> {
    let mut variables = launch
        .encode_env()
        .with_context(|| {
            format!(
                "failed to encode launch environment for participant {}",
                launch.participant_id
            )
        })?
        .into_iter()
        .map(|(key, value)| (key.to_string(), value))
        .collect::<BTreeMap<_, _>>();
    validate_config_size(launch, &variables)?;
    variables.remove(env::EXECUTION_ORIGIN);
    variables.remove(env::CLOCK);
    Ok(EncodedParticipantEnv { variables })
}

fn validate_config_size(
    launch: &ParticipantLaunch,
    variables: &BTreeMap<String, String>,
) -> Result<()> {
    let Some(encoded) = variables.get(env::CONFIG) else {
        return Ok(());
    };
    let size = encoded.len();
    if size > MAX_CONFIG_ENV_BYTES {
        bail!(
            "participant {} PHOXAL_CONFIG is {size} bytes, above the {MAX_CONFIG_ENV_BYTES} byte limit",
            launch.participant_id
        );
    }
    Ok(())
}

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
    for (env_key, flag) in ENV_TO_FLAG {
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

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use phoxal_runtime_contract::{BusProfile, ClockMode};

    use super::*;

    fn launch(participant_id: &str) -> ParticipantLaunch {
        ParticipantLaunch {
            participant_id: participant_id.to_string(),
            execution: crate::identity::ExecutionId::mint(),
            producer: crate::identity::ProducerId::mint(),
            execution_origin: None,
            namespace: "dev".to_string(),
            robot_id: "testbot".to_string(),
            bus: BusProfile {
                connect_endpoints: vec!["tcp/localhost:7447".to_string()],
            },
            clock: ClockMode::Real,
            config: None,
            bundle_root: Some(PathBuf::from("/tmp/phoxal/robot")),
            component_instance: None,
            shutdown_grace_ms: phoxal_runtime_contract::DEFAULT_SHUTDOWN_GRACE_MS,
        }
    }

    #[test]
    fn participant_env_config_is_compact_escaped_json() -> anyhow::Result<()> {
        let mut launch = launch("mission");
        launch.config = Some(serde_json::json!({
            "message": "quoted \"value\" and backslash \\ with newline\nvisible",
            "path": "/tmp/phoxal/robot's model",
        }));
        let encoded = encode_participant_env(&launch)?;
        assert_eq!(
            encoded.variables().get(env::CONFIG).map(String::as_str),
            Some(
                r#"{"message":"quoted \"value\" and backslash \\ with newline\nvisible","path":"/tmp/phoxal/robot's model"}"#
            )
        );
        Ok(())
    }

    #[test]
    fn tool_environment_is_clockless() -> anyhow::Result<()> {
        let mut launch = launch("tool-log");
        launch.execution_origin = Some(phoxal_runtime_contract::ExecutionOrigin::mint());
        let encoded = encode_tool_env(&launch)?;
        assert!(!encoded.variables().contains_key(env::CLOCK));
        assert_eq!(
            encoded.variables().get(env::EXECUTION_ORIGIN),
            None,
            "a tool must not receive the origin that would let it reconstruct robot time"
        );
        assert_eq!(
            encoded
                .variables()
                .get(env::PARTICIPANT_ID)
                .map(String::as_str),
            Some("tool-log")
        );
        Ok(())
    }

    #[test]
    fn oversized_config_names_participant_size_and_limit() {
        let mut launch = launch("huge_config");
        launch.config = Some(serde_json::json!({
            "blob": "x".repeat(MAX_CONFIG_ENV_BYTES),
        }));
        let error = encode_participant_env(&launch).expect_err("config should exceed the limit");
        let message = error.to_string();
        assert!(message.contains("huge_config"), "{message}");
        assert!(
            message.contains(&MAX_CONFIG_ENV_BYTES.to_string()),
            "{message}"
        );
        assert!(message.contains("bytes"), "{message}");
    }
}
