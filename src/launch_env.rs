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
    pub(crate) fn from_variables(variables: BTreeMap<String, String>) -> Self {
        Self { variables }
    }

    #[must_use]
    #[cfg(test)]
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

/// The common `PHOXAL_*` env names paired with their equivalent `--flag`, in
/// the exact order every framework participant launch policy declares them.
/// Webots controller arguments use only this clockless surface.
pub const COMMON_ENV_TO_FLAG: &[(&str, &str)] = &[
    (env::PARTICIPANT_ID, "--participant-id"),
    (env::ROBOT_ID, "--robot-id"),
    (env::NAMESPACE, "--namespace"),
    (env::ROBOT_ROOT, "--robot-root"),
    (env::COMPONENT_INSTANCE, "--component-instance"),
    (env::CONNECT, "--connect"),
    (env::CONFIG, "--config"),
];

/// Full environment/flag map for clock-selectable services and drivers. Manual
/// command rendering uses this; simulator controller arguments deliberately do
/// not.
pub const ENV_TO_FLAG: &[(&str, &str)] = &[
    (env::PARTICIPANT_ID, "--participant-id"),
    (env::ROBOT_ID, "--robot-id"),
    (env::NAMESPACE, "--namespace"),
    (env::ROBOT_ROOT, "--robot-root"),
    (env::COMPONENT_INSTANCE, "--component-instance"),
    (env::CONNECT, "--connect"),
    (env::CONFIG, "--config"),
    (env::CLOCK, "--clock"),
];

/// Render a participant's launch contract as the exact argv the framework
/// simulator launch policy parses: `--participant-id`, `--robot-id`, and
/// `--namespace` are always emitted; `--robot-root`,
/// `--component-instance`, `--connect` (`bus.connect_endpoints` joined with
/// `,`), and `--config` (compact JSON, one argv element) only when present.
///
/// Webots controllers and the supervisor receive their whole launch contract
/// this way, never via env: a Webots controller process inherits the Webots
/// app's environment, so `PHOXAL_*` env vars would be shared across the
/// supervisor and every controller. `controllerArgs` (argv) is per-node, so it
/// is the only way to give each simulation participant its own contract.
///
/// Uses the same common-field encoder as [`encode_participant_env`], so
/// `--config` is capped by [`MAX_CONFIG_ENV_BYTES`] the same way (a
/// supervisor's spawn list can grow large); exceeding it is an error naming
/// the participant.
pub fn to_controller_args(launch: &ParticipantLaunch) -> Result<Vec<String>> {
    let variables = encode_common_participant_variables(launch)?;
    let mut args = Vec::with_capacity(COMMON_ENV_TO_FLAG.len() * 2);
    for (env_key, flag) in COMMON_ENV_TO_FLAG {
        if let Some(value) = variables.get(*env_key) {
            args.push((*flag).to_string());
            args.push(value.clone());
        }
    }
    Ok(args)
}

pub fn encode_participant_env(launch: &ParticipantLaunch) -> Result<EncodedParticipantEnv> {
    let mut variables = encode_common_participant_variables(launch)?;
    variables.insert(env::CLOCK.to_string(), launch.clock.to_string());
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
            crate::human::bytes(size as u64),
            crate::human::bytes(MAX_CONFIG_ENV_BYTES as u64)
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
            namespace: "dev".to_string(),
            robot_id: "robot_v1".to_string(),
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
    fn oversized_config_names_participant_size_and_limit() {
        let launch = ParticipantLaunch {
            participant_id: "huge_config".to_string(),
            namespace: "dev".to_string(),
            robot_id: "robot_v1".to_string(),
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

    #[test]
    fn to_controller_args_emits_the_exact_flag_set() -> anyhow::Result<()> {
        let launch = ParticipantLaunch {
            participant_id: "simulator-webots-controller-robot_v1".to_string(),
            namespace: "dev".to_string(),
            robot_id: "robot_v1".to_string(),
            bus: BusProfile {
                connect_endpoints: vec!["tcp/localhost:7447".to_string()],
            },
            // Simulator controller arguments ignore this generic launch-record
            // field; simulator binaries are structurally clockless.
            clock: ClockMode::Simulation,
            config: Some(serde_json::json!({"require_native": true})),
            robot_root: Some(PathBuf::from("/tmp/phoxal/robot")),
            component_instance: Some("left_drive".to_string()),
            shutdown_grace_ms: phoxal::participant::launch::DEFAULT_SHUTDOWN_GRACE_MS,
        };

        let args = to_controller_args(&launch)?;

        assert_eq!(
            args,
            vec![
                "--participant-id".to_string(),
                "simulator-webots-controller-robot_v1".to_string(),
                "--robot-id".to_string(),
                "robot_v1".to_string(),
                "--namespace".to_string(),
                "dev".to_string(),
                "--robot-root".to_string(),
                "/tmp/phoxal/robot".to_string(),
                "--component-instance".to_string(),
                "left_drive".to_string(),
                "--connect".to_string(),
                "tcp/localhost:7447".to_string(),
                "--config".to_string(),
                r#"{"require_native":true}"#.to_string(),
            ]
        );

        // The framework runner's clap `LaunchCli` (phoxal::participant::launch)
        // parses exactly this flag/value shape; its struct is private to the
        // `phoxal` crate so it cannot be constructed here, but a clap parser
        // with the identical long-flag surface (mirroring
        // `render_manual_command_line` in supervisor.rs) proves the argv shape
        // round-trips through clap's derive parsing the same way.
        #[derive(clap::Parser)]
        struct AssertLaunchCliShape {
            #[arg(long)]
            participant_id: String,
            #[arg(long)]
            robot_id: String,
            #[arg(long)]
            namespace: String,
            #[arg(long)]
            robot_root: Option<String>,
            #[arg(long)]
            component_instance: Option<String>,
            #[arg(long)]
            connect: Option<String>,
            #[arg(long)]
            config: Option<String>,
        }
        use clap::Parser;
        let mut argv = vec!["phoxal-simulator-webots-controller".to_string()];
        argv.extend(args);
        let parsed = AssertLaunchCliShape::try_parse_from(&argv)?;
        assert_eq!(
            parsed.participant_id,
            "simulator-webots-controller-robot_v1"
        );
        assert_eq!(parsed.robot_id, "robot_v1");
        assert_eq!(parsed.namespace, "dev");
        assert_eq!(parsed.robot_root.as_deref(), Some("/tmp/phoxal/robot"));
        assert_eq!(parsed.component_instance.as_deref(), Some("left_drive"));
        assert_eq!(parsed.connect.as_deref(), Some("tcp/localhost:7447"));
        assert_eq!(parsed.config.as_deref(), Some(r#"{"require_native":true}"#));
        Ok(())
    }

    #[test]
    fn to_controller_args_omits_absent_optional_fields() -> anyhow::Result<()> {
        let launch = ParticipantLaunch {
            participant_id: "simulator-webots-supervisor".to_string(),
            namespace: "dev".to_string(),
            robot_id: "robot_v1".to_string(),
            bus: BusProfile::default(),
            clock: ClockMode::Simulation,
            config: None,
            robot_root: None,
            component_instance: None,
            shutdown_grace_ms: phoxal::participant::launch::DEFAULT_SHUTDOWN_GRACE_MS,
        };

        let args = to_controller_args(&launch)?;

        assert_eq!(
            args,
            vec![
                "--participant-id".to_string(),
                "simulator-webots-supervisor".to_string(),
                "--robot-id".to_string(),
                "robot_v1".to_string(),
                "--namespace".to_string(),
                "dev".to_string(),
            ]
        );
        Ok(())
    }

    #[test]
    fn to_controller_args_rejects_oversized_config() {
        let launch = ParticipantLaunch {
            participant_id: "simulator-webots-supervisor".to_string(),
            namespace: "dev".to_string(),
            robot_id: "robot_v1".to_string(),
            bus: BusProfile::default(),
            clock: ClockMode::Simulation,
            config: Some(serde_json::json!({"blob": "x".repeat(MAX_CONFIG_ENV_BYTES)})),
            robot_root: None,
            component_instance: None,
            shutdown_grace_ms: phoxal::participant::launch::DEFAULT_SHUTDOWN_GRACE_MS,
        };

        let error = to_controller_args(&launch).expect_err("config should exceed the limit");
        let message = error.to_string();
        assert!(message.contains("simulator-webots-supervisor"), "{message}");
        assert!(
            message.contains(&MAX_CONFIG_ENV_BYTES.to_string()),
            "{message}"
        );
    }
}
