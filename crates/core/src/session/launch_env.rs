//! Stable participant launch-environment encoding.

use std::collections::BTreeMap;

use anyhow::{Context, Result, bail};
use phoxal::participant::launch::{ParticipantLaunch, env};

/// Keep participant configs comfortably below Linux's 128 KiB per-env-string
/// ceiling and leave room for the key name plus the rest of the launch env.
pub const MAX_CONFIG_ENV_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedParticipantEnv {
    variables: BTreeMap<String, String>,
}

impl EncodedParticipantEnv {
    #[must_use]
    pub fn from_variables(variables: BTreeMap<String, String>) -> Self {
        Self { variables }
    }

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

    #[must_use]
    pub fn environment_file(&self) -> String {
        let mut rendered = String::new();
        for (key, value) in &self.variables {
            rendered.push_str(key);
            rendered.push('=');
            rendered.push_str(&escape_environment_file_value(value));
            rendered.push('\n');
        }
        rendered
    }
}

/// Full environment/flag map for clock-selectable services and drivers. Manual
/// command rendering uses this; simulator controller arguments deliberately do
/// not.
pub const ENV_TO_FLAG: &[(&str, &str)] = &[
    (env::PARTICIPANT_ID, "--participant-id"),
    (env::ROBOT_ID, "--robot-id"),
    (env::NAMESPACE, "--namespace"),
    (env::ROBOT_ROOT, "--robot-root"),
    (env::COMPONENT_INSTANCE, "--component-instance"),
    (env::EXECUTION_ID, "--execution-id"),
    (env::PRODUCER_ID, "--producer-id"),
    (env::EXECUTION_ORIGIN, "--execution-origin"),
    (env::CONNECT, "--connect"),
    (env::CONFIG, "--config"),
    (env::CLOCK, "--clock"),
];

pub fn encode_participant_env(launch: &ParticipantLaunch) -> Result<EncodedParticipantEnv> {
    let mut variables = encode_common_participant_variables(launch)?;
    variables.insert(env::CLOCK.to_string(), launch.clock.to_string());
    Ok(EncodedParticipantEnv { variables })
}

/// A tool joins the *execution*, not the clock (#952 section B). It therefore
/// carries the execution and its own producer identity like every bus
/// participant, but never the execution origin: the origin is what turns a host
/// boot-clock reading into an exact `RobotInstant`, and a tool must not be able
/// to reconstruct one.
pub fn encode_tool_env(launch: &ParticipantLaunch) -> Result<EncodedParticipantEnv> {
    let mut variables = encode_common_participant_variables(launch)?;
    variables.remove(env::EXECUTION_ORIGIN);
    Ok(EncodedParticipantEnv { variables })
}

fn encode_common_participant_variables(
    launch: &ParticipantLaunch,
) -> Result<BTreeMap<String, String>> {
    let mut variables = BTreeMap::new();
    variables.insert(
        env::PARTICIPANT_ID.to_string(),
        launch.participant_id.clone(),
    );
    variables.insert(env::ROBOT_ID.to_string(), launch.robot_id.clone());
    variables.insert(env::NAMESPACE.to_string(), launch.namespace.clone());
    if let Some(robot_root) = &launch.robot_root {
        variables.insert(
            env::ROBOT_ROOT.to_string(),
            robot_root.display().to_string(),
        );
    }
    if let Some(component_instance) = &launch.component_instance {
        variables.insert(
            env::COMPONENT_INSTANCE.to_string(),
            component_instance.clone(),
        );
    }
    // Every participant carries the supervised run and its own producer
    // identity (#952 section B/G); the origin is present whenever the
    // supervisor minted one.
    variables.insert(env::EXECUTION_ID.to_string(), launch.execution.to_string());
    variables.insert(env::PRODUCER_ID.to_string(), launch.producer.to_string());
    if let Some(origin) = launch.execution_origin {
        variables.insert(env::EXECUTION_ORIGIN.to_string(), origin.encode());
    }
    if !launch.bus.connect_endpoints.is_empty() {
        variables.insert(
            env::CONNECT.to_string(),
            launch.bus.connect_endpoints.join(","),
        );
    }
    // The framework runner treats an absent (or empty) PHOXAL_CONFIG as "no
    // config" and only deserializes when the variable is present - encoding
    // `{}` for a config-less participant makes unit-config services fail with
    // "invalid type: map, expected unit".
    if let Some(config) = compact_config_json(launch)? {
        variables.insert(env::CONFIG.to_string(), config);
    }
    Ok(variables)
}

fn compact_config_json(launch: &ParticipantLaunch) -> Result<Option<String>> {
    let Some(config) = launch.config.clone() else {
        return Ok(None);
    };
    let encoded = serde_json::to_string(&config).with_context(|| {
        format!(
            "failed to encode PHOXAL_CONFIG for participant {}",
            launch.participant_id
        )
    })?;
    let size = encoded.len();
    if size > MAX_CONFIG_ENV_BYTES {
        bail!(
            "participant {} PHOXAL_CONFIG is {} ({size} bytes), above the {} ({MAX_CONFIG_ENV_BYTES} byte limit)",
            launch.participant_id,
            super::human::bytes(size as u64),
            super::human::bytes(MAX_CONFIG_ENV_BYTES as u64)
        );
    }
    Ok(Some(encoded))
}

fn escape_environment_file_value(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len() + 2);
    escaped.push('"');
    for ch in value.chars() {
        match ch {
            '"' => escaped.push_str("\\\""),
            '\\' => escaped.push_str("\\\\"),
            '$' => escaped.push_str("\\$"),
            '`' => escaped.push_str("\\`"),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            _ => escaped.push(ch),
        }
    }
    escaped.push('"');
    escaped
}

#[cfg(test)]
pub(crate) fn parse_environment_file_for_tests(
    contents: &str,
) -> anyhow::Result<BTreeMap<String, String>> {
    let mut parsed = BTreeMap::new();
    for (line_index, line) in contents.lines().enumerate() {
        let Some((key, value)) = line.split_once('=') else {
            bail!("line {} is not KEY=VALUE", line_index + 1);
        };
        parsed.insert(key.to_string(), parse_quoted_value_for_tests(value)?);
    }
    Ok(parsed)
}

#[cfg(test)]
fn parse_quoted_value_for_tests(value: &str) -> anyhow::Result<String> {
    let Some(inner) = value
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
    else {
        bail!("value is not double quoted: {value}");
    };
    let mut parsed = String::new();
    let mut chars = inner.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            parsed.push(ch);
            continue;
        }
        match chars.next() {
            Some('"') => parsed.push('"'),
            Some('\\') => parsed.push('\\'),
            Some('$') => parsed.push('$'),
            Some('`') => parsed.push('`'),
            Some('n') => parsed.push('\n'),
            Some('r') => parsed.push('\r'),
            Some(other) => {
                bail!("unsupported escape in test EnvironmentFile parser: \\{other}");
            }
            None => bail!("trailing escape in EnvironmentFile value"),
        }
    }
    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use phoxal::participant::launch::{BusProfile, ClockMode};

    use super::*;

    #[test]
    fn spawn_env_and_environment_file_carry_identical_variables() -> anyhow::Result<()> {
        let launch = ParticipantLaunch {
            participant_id: "mission".to_string(),
            execution: phoxal::bus::ExecutionId::mint(),
            producer: phoxal::bus::ProducerId::mint(),
            execution_origin: None,
            namespace: "dev".to_string(),
            robot_id: "testbot".to_string(),
            bus: BusProfile {
                connect_endpoints: vec!["tcp/localhost:7447".to_string()],
            },
            clock: ClockMode::Real,
            config: Some(serde_json::json!({
                "message": "quoted \"value\" and backslash \\ with newline\nvisible",
                "path": "/tmp/phoxal/robot's model",
            })),
            robot_root: Some(PathBuf::from("/tmp/phoxal/robot")),
            component_instance: None,
            shutdown_grace_ms: phoxal::participant::launch::DEFAULT_SHUTDOWN_GRACE_MS,
        };

        let encoded = encode_participant_env(&launch)?;
        let from_env_file = parse_environment_file_for_tests(&encoded.environment_file())?;

        assert_eq!(&from_env_file, encoded.variables());
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
        let launch = ParticipantLaunch {
            participant_id: "tool-log".to_string(),
            execution: phoxal::bus::ExecutionId::mint(),
            producer: phoxal::bus::ProducerId::mint(),
            execution_origin: Some(phoxal::participant::ExecutionOrigin::mint()),
            namespace: "dev".to_string(),
            robot_id: "testbot".to_string(),
            bus: BusProfile {
                connect_endpoints: vec!["tcp/localhost:7447".to_string()],
            },
            clock: ClockMode::Real,
            config: None,
            robot_root: Some(PathBuf::from("/tmp/phoxal/robot")),
            component_instance: None,
            shutdown_grace_ms: phoxal::participant::launch::DEFAULT_SHUTDOWN_GRACE_MS,
        };

        let encoded = encode_tool_env(&launch)?;
        assert!(!encoded.variables().contains_key(env::CLOCK));
        assert_eq!(
            encoded
                .variables()
                .get(env::PARTICIPANT_ID)
                .map(String::as_str),
            Some("tool-log")
        );
        assert_eq!(
            encoded.variables().get(env::CONNECT).map(String::as_str),
            Some("tcp/localhost:7447")
        );
        assert_eq!(
            encoded.variables().get(env::EXECUTION_ORIGIN),
            None,
            "a tool must not receive the origin that would let it reconstruct robot time"
        );
        // Every participant carries the supervised run and its own producer.
        assert_eq!(
            encoded
                .variables()
                .get(env::EXECUTION_ID)
                .map(String::as_str),
            Some(launch.execution.to_string().as_str())
        );
        assert_eq!(
            encoded
                .variables()
                .get(env::PRODUCER_ID)
                .map(String::as_str),
            Some(launch.producer.to_string().as_str())
        );
        Ok(())
    }

    #[test]
    fn oversized_config_names_participant_size_and_limit() {
        let launch = ParticipantLaunch {
            participant_id: "huge_config".to_string(),
            execution: phoxal::bus::ExecutionId::mint(),
            producer: phoxal::bus::ProducerId::mint(),
            execution_origin: None,
            namespace: "dev".to_string(),
            robot_id: "testbot".to_string(),
            bus: BusProfile::default(),
            clock: ClockMode::Real,
            config: Some(serde_json::json!({
                "blob": "x".repeat(MAX_CONFIG_ENV_BYTES),
            })),
            robot_root: None,
            component_instance: None,
            shutdown_grace_ms: phoxal::participant::launch::DEFAULT_SHUTDOWN_GRACE_MS,
        };

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
