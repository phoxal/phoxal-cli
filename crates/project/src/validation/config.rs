//! Config responsibilities for check.

use crate::check as graph_check;
use phoxal::authoring::source::robot::v0::Manifest as RobotManifest;
use serde_json::Value;

pub(crate) fn validate_user_service_config(
    service_id: &str,
    schema: Option<&Value>,
    robot: Option<&RobotManifest>,
) -> Option<graph_check::Problem> {
    // An absent manifest config is `null`, not `{}`: a no-config service's
    // emitted schema requires null (so absent passes), while a service with a
    // real object schema still fails correctly as config-required-but-missing.
    let config = robot
        .and_then(|robot| robot.services.get(service_id))
        .and_then(|service| service.config.clone());
    validate_user_runtime_config(service_id, schema, config.as_ref(), "services")
}

/// Validate one declared user runtime's authored config against its embedded
/// schema, with the declaring map (`services`) in the diagnostic path.
/// The config value is passed in, never looked up again.
pub(crate) fn validate_user_runtime_config(
    runtime_id: &str,
    schema: Option<&Value>,
    config: Option<&Value>,
    family: &str,
) -> Option<graph_check::Problem> {
    let schema = schema?;
    let config = config.cloned().unwrap_or(Value::Null);
    let errors = validate_json_schema(schema, &config, &format!("{family}.{runtime_id}.config"));
    if errors.is_empty() {
        None
    } else {
        Some(graph_check::Problem::InvalidConfig {
            runtime_id: runtime_id.to_string(),
            errors,
        })
    }
}

pub(crate) fn validate_json_schema(schema: &Value, value: &Value, path: &str) -> Vec<String> {
    let validator = match jsonschema::options()
        .with_draft(jsonschema::Draft::Draft202012)
        .build(schema)
    {
        Ok(validator) => validator,
        Err(error) => {
            return vec![format!("{path}: emitted config_schema is invalid: {error}")];
        }
    };

    validator
        .iter_errors(value)
        .map(|error| {
            let instance_path = error.instance_path().to_string();
            if instance_path.is_empty() {
                format!("{path}: {error}")
            } else {
                format!("{path}{instance_path}: {error}")
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use crate::check::Problem;
    use crate::check::source::SourceParticipant;
    use crate::validation::{
        CheckGraphContext, CheckOutcome, RawArtifact, RawParticipantReport, run_check_with_context,
    };
    use anyhow::{Result, bail};
    use phoxal::authoring::source::robot::v0::Manifest as Robot;
    use serde_json::Value;
    use std::path::PathBuf;

    const LAUNCH_PLAN_FIXTURE_ROBOT: &str = r#"schema: phoxal/robot/v0
robot:
  id: testbot
  motion_limits:
    max_linear_speed_mps: 0.6
    max_angular_speed_radps: 2.0
  kinematic:
    kind: differential
    left_actuators: [left_drive.motor]
    right_actuators: [right_drive.motor]
    left_encoders: [left_drive.encoder]
    right_encoders: [right_drive.encoder]
    wheel_radius_m: 0.1
    wheel_base_m: 0.5
  components:
    left_drive:
      component: ddsm115
      mount_link: left_wheel
      driver:
        connection: { type: can, bus: 0, node_id: 1 }
    right_drive:
      component: ddsm115
      mount_link: right_wheel
      driver:
        connection: { type: can, bus: 0, node_id: 2 }
services:
  mission: {}
"#;

    fn robot_with_service_config(service_id: &str, config: Value) -> Result<Robot> {
        let mut robot = crate::source::resolver::parse_robot_from_string(
            &LAUNCH_PLAN_FIXTURE_ROBOT.replace("mission", service_id),
        )?;
        robot
            .services
            .get_mut(service_id)
            .expect("fixture service")
            .config = Some(config);
        Ok(robot)
    }

    fn raw(id: &str) -> RawParticipantReport {
        RawParticipantReport {
            artifact: RawArtifact {
                kind: "service".to_string(),
                id: id.to_string(),
            },
            config_schema: None,
        }
    }

    #[test]
    fn user_service_config_is_validated_against_emitted_schema() -> Result<()> {
        let sources = vec![SourceParticipant::user_service(
            "avoid".to_string(),
            PathBuf::from("/fake/project/runtimes/avoid"),
        )];
        let emitted = RawParticipantReport {
            artifact: RawArtifact {
                kind: "service".to_string(),
                id: "avoid".to_string(),
            },
            config_schema: Some(serde_json::json!({
                "$schema": "https://json-schema.org/draft/2020-12/schema",
                "title": "Config",
                "type": "object",
                "properties": { "gain": { "type": "number", "format": "double" } },
                "required": ["gain"]
            })),
        };
        assert_eq!(
            emitted.config_schema,
            Some(serde_json::json!({
                "$schema": "https://json-schema.org/draft/2020-12/schema",
                "title": "Config",
                "type": "object",
                "properties": { "gain": { "type": "number", "format": "double" } },
                "required": ["gain"]
            }))
        );

        let check_config = |config: Value| -> Result<CheckOutcome> {
            let robot = robot_with_service_config("avoid", config)?;

            run_check_with_context(
                &[],
                &sources,
                CheckGraphContext {
                    robot: Some(&robot),
                },
                |_| bail!("no platform images should be fetched"),
                |_| Ok(emitted.clone()),
            )
        };

        let missing = check_config(serde_json::json!({}))?;
        assert!(matches!(
            missing
                .report
                .problems
                .iter()
                .find(|problem| matches!(problem, Problem::InvalidConfig { .. })),
            Some(Problem::InvalidConfig { runtime_id, errors })
                if runtime_id == "avoid"
                    && errors.iter().any(|error| error.contains("gain"))
        ));

        let mistyped = check_config(serde_json::json!({ "gain": "fast" }))?;
        assert!(matches!(
            mistyped
                .report
                .problems
                .iter()
                .find(|problem| matches!(problem, Problem::InvalidConfig { .. })),
            Some(Problem::InvalidConfig { runtime_id, errors })
                if runtime_id == "avoid"
                    && errors.iter().any(|error| error.contains("gain"))
        ));

        let valid = check_config(serde_json::json!({ "gain": 1.5 }))?;
        assert!(
            valid
                .report
                .problems
                .iter()
                .all(|problem| !matches!(problem, Problem::InvalidConfig { .. })),
            "{:?}",
            valid.report.problems
        );
        Ok(())
    }

    #[test]
    fn absent_user_service_config_validates_as_null() -> Result<()> {
        let sources = vec![SourceParticipant::user_service(
            "optional".to_string(),
            PathBuf::from("/fake/project/runtimes/optional"),
        )];

        let outcome = run_check_with_context(
            &[],
            &sources,
            CheckGraphContext { robot: None },
            |_| bail!("no platform images should be fetched"),
            |_| {
                let mut raw = raw("optional");
                raw.config_schema = Some(serde_json::json!({ "type": "null" }));
                Ok(raw)
            },
        )?;

        assert!(
            outcome
                .report
                .problems
                .iter()
                .all(|problem| !matches!(problem, Problem::InvalidConfig { .. })),
            "{:?}",
            outcome.report.problems
        );
        Ok(())
    }

    #[test]
    fn absent_user_service_config_still_fails_required_object_schema() -> Result<()> {
        let sources = vec![SourceParticipant::user_service(
            "required".to_string(),
            PathBuf::from("/fake/project/runtimes/required"),
        )];

        let outcome = run_check_with_context(
            &[],
            &sources,
            CheckGraphContext { robot: None },
            |_| bail!("no platform images should be fetched"),
            |_| {
                let mut raw = raw("required");
                raw.config_schema = Some(serde_json::json!({
                    "type": "object",
                    "required": ["gain"],
                    "properties": {
                        "gain": { "type": "number" }
                    },
                    "additionalProperties": false
                }));
                Ok(raw)
            },
        )?;

        assert!(matches!(
            outcome
                .report
                .problems
                .iter()
                .find(|problem| matches!(problem, Problem::InvalidConfig { .. })),
            Some(Problem::InvalidConfig { runtime_id, errors })
                if runtime_id == "required"
                    && errors.iter().any(|error| error.contains("null"))
        ));
        Ok(())
    }

    #[test]
    fn user_service_config_uses_full_json_schema_keywords() -> Result<()> {
        let sources = vec![SourceParticipant::user_service(
            "avoid".to_string(),
            PathBuf::from("/fake/project/runtimes/avoid"),
        )];
        let robot = robot_with_service_config(
            "avoid",
            serde_json::json!({
                "gains": [0.25, 5.5],
                "mode": "FAST",
                "extra": true
            }),
        )?;

        let outcome = run_check_with_context(
            &[],
            &sources,
            CheckGraphContext {
                robot: Some(&robot),
            },
            |_| bail!("no platform images should be fetched"),
            |_| {
                let mut raw = raw("avoid");
                raw.config_schema = Some(serde_json::json!({
                    "$schema": "https://json-schema.org/draft/2020-12/schema",
                    "type": "object",
                    "required": ["gains", "mode"],
                    "properties": {
                        "gains": {
                            "type": "array",
                            "minItems": 2,
                            "maxItems": 2,
                            "items": { "$ref": "#/$defs/gain" }
                        },
                        "mode": {
                            "type": "string",
                            "pattern": "^[a-z]+$"
                        }
                    },
                    "additionalProperties": false,
                    "$defs": {
                        "gain": {
                            "type": "number",
                            "minimum": 0.0,
                            "maximum": 1.0
                        }
                    }
                }));
                Ok(raw)
            },
        )?;

        let [Problem::InvalidConfig { runtime_id, errors }] = outcome.report.problems.as_slice()
        else {
            panic!(
                "expected one InvalidConfig problem, got {:?}",
                outcome.report.problems
            );
        };
        assert_eq!(runtime_id, "avoid");
        assert!(
            errors.iter().any(|error| error.contains("/gains/1")),
            "{errors:?}"
        );
        assert!(
            errors.iter().any(|error| error.contains("/mode")),
            "{errors:?}"
        );
        assert!(
            errors
                .iter()
                .any(|error| error.to_ascii_lowercase().contains("additional properties")),
            "{errors:?}"
        );
        Ok(())
    }
}
