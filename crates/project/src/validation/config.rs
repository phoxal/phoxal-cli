//! Config responsibilities for check.

use phoxal_cli_core::check as graph_check;
use phoxal_manifest::source::robot::v0::Manifest as RobotManifest;
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
/// schema, with the declaring map (`services`/`tools`) in the diagnostic path.
/// The config VALUE is passed in, never re-looked-up, so tool declarations
/// validate their real `tools.<id>.config` (#950).
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
