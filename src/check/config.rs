//! Config responsibilities for check.

use super::RawEmitApis;
use phoxal::check as graph_check;
use phoxal_cli_core::check::participant_metadata;
use phoxal_cli_core::project::resolver::RobotManifestExtras;
use serde_json::Value;

pub(crate) fn contract_surface(
    raw: &RawEmitApis,
    participant_id: String,
) -> graph_check::ParticipantContractSurface {
    graph_check::ParticipantContractSurface {
        participant_id,
        contracts: raw
            .required_contracts
            .iter()
            .map(|contract| participant_metadata::ParticipantMetaContract {
                role: contract.role.clone(),
                version: contract.version.clone(),
                contract: contract.contract.clone(),
                external: contract.external,
            })
            .collect(),
    }
}

pub(crate) fn validate_user_service_config(
    service_id: &str,
    schema: Option<&Value>,
    manifest_extras: &RobotManifestExtras,
) -> Option<graph_check::Problem> {
    let schema = schema?;
    // An absent manifest config is `null`, not `{}`: a no-config service's
    // emitted schema requires null (so absent passes), while a service with a
    // real object schema still fails correctly as config-required-but-missing.
    let config = manifest_extras
        .user_runtime_config(service_id)
        .cloned()
        .unwrap_or(Value::Null);
    let errors = validate_json_schema(schema, &config, &format!("services.{service_id}.config"));
    if errors.is_empty() {
        None
    } else {
        Some(graph_check::Problem::InvalidConfig {
            runtime_id: service_id.to_string(),
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
