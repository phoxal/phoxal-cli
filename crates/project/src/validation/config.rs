//! Config responsibilities for check.

use crate::check as graph_check;
use anyhow::{Context, Result};
use phoxal::authoring::source::robot::v0::Manifest as RobotManifest;
use phoxal::model::connection::ConnectionKind;
use serde_json::Value;

/// Validate one declared `services.<id>` entry's `config` against the schema
/// the binary that reads it embeds.
///
/// The `services:` map is where an operator both declares a service this
/// project owns the source of and configures an official one that runs either
/// way, so every key in it is checked the same way against the same slot.
/// A key the document does not carry is not checked at all: an official
/// service with no entry has nothing authored to be wrong about, and a service
/// this project builds is only ever resolved because the document declared it.
///
/// An entry whose `config:` is absent is `null`, not `{}`: a no-config
/// service's emitted schema requires null (so an empty entry passes), while a
/// service with a real object schema still fails correctly as
/// config-required-but-missing.
pub(crate) fn validate_declared_service_config(
    service_id: &str,
    schema: Option<&Value>,
    robot: Option<&RobotManifest>,
) -> Option<graph_check::Problem> {
    let declared = robot?.services.get(service_id)?;
    validate_authored_config(
        graph_check::ConfigOwner::Service,
        service_id,
        schema,
        declared.config.as_ref(),
        &format!("services.{service_id}.config"),
    )
}

/// Validate one driven component instance's whole `driver:` block against the
/// contract its driver binary embeds: the driver-owned `config` against that
/// binary's emitted schema, and the authored `connection` against the one kind
/// the binary declared it accepts.
///
/// Both halves are the same failure seen at two moments. A driver launched
/// against a block it cannot read refuses to start before its bus opens, so
/// every path that already holds the binary's contract - `validate`, `build`,
/// `run` - owes the operator that answer while the project is still on the
/// desk.
///
/// # Errors
///
/// When the authored document declares no `driver:` block for an instance a
/// driver participant was nonetheless resolved for. That is resolution
/// disagreeing with the document it resolved from, not a user mistake.
pub(crate) fn validate_component_driver_block(
    instance: &str,
    artifact_id: &str,
    schema: Option<&Value>,
    declared_connection: Option<ConnectionKind>,
    robot: Option<&RobotManifest>,
) -> Result<Vec<graph_check::Problem>> {
    // No authored document in hand means there is nothing to compare the
    // binary against; the callers that check driver blocks always carry one.
    let Some(robot) = robot else {
        return Ok(Vec::new());
    };
    let driver = robot
        .robot
        .components
        .get(instance)
        .and_then(|component| component.driver.as_ref())
        .with_context(|| {
            format!(
                "component driver '{artifact_id}' was resolved for component instance \
                 '{instance}', but the authored robot document declares no driver block under \
                 robot.components.{instance}"
            )
        })?;

    let mut problems = Vec::new();
    if let Some(problem) = validate_authored_config(
        graph_check::ConfigOwner::ComponentDriver,
        instance,
        schema,
        driver.config.as_ref(),
        &format!("components.{instance}.driver.config"),
    ) {
        problems.push(problem);
    }
    // A driver that declared no kind takes the whole connection and decides
    // for itself, so there is nothing to disagree with.
    if let Some(declared) = declared_connection
        && driver.connection.kind() != declared
    {
        problems.push(graph_check::Problem::ConnectionKindMismatch {
            instance: instance.to_string(),
            artifact_id: artifact_id.to_string(),
            declared,
            authored: driver.connection.kind(),
        });
    }
    Ok(problems)
}

/// Validate one authored config value against the embedded schema of the
/// binary that will read it, reporting failures under `path` - the exact
/// authored location an operator edits (`services.avoid.config`,
/// `components.left_drive.driver.config`). The config value is passed in,
/// never looked up again.
pub(crate) fn validate_authored_config(
    owner: graph_check::ConfigOwner,
    runtime_id: &str,
    schema: Option<&Value>,
    config: Option<&Value>,
    path: &str,
) -> Option<graph_check::Problem> {
    let schema = schema?;
    let config = config.cloned().unwrap_or(Value::Null);
    let errors = validate_json_schema(schema, &config, path);
    if errors.is_empty() {
        None
    } else {
        Some(graph_check::Problem::InvalidConfig {
            owner,
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
    use crate::check::source::SourceParticipant;
    use crate::check::{ConfigOwner, Problem};
    use crate::validation::{
        CheckGraphContext, CheckOutcome, PlatformArtifactRef, RawArtifact, RawParticipantReport,
        run_check_with_context,
    };
    use anyhow::{Result, bail};
    use phoxal::authoring::source::robot::v0::Manifest as Robot;
    use phoxal::model::connection::ConnectionKind;
    use phoxal_cli_catalog::ArtifactKind;
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
        config: { reduction: 20 }
    right_drive:
      component: ddsm115
      mount_link: right_wheel
      driver:
        connection: { type: can, bus: 0, node_id: 2 }
services:
  mission: {}
"#;

    /// The fixture document with its one `services:` entry renamed, declaring
    /// `service_id` and authoring nothing under it.
    fn robot_declaring(service_id: &str) -> Result<Robot> {
        crate::source::resolver::parse_robot_from_string(
            &LAUNCH_PLAN_FIXTURE_ROBOT.replace("mission", service_id),
        )
    }

    fn robot_with_service_config(service_id: &str, config: Value) -> Result<Robot> {
        let mut robot = robot_declaring(service_id)?;
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
            connection: None,
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
            connection: None,
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
            Some(Problem::InvalidConfig {
                owner: ConfigOwner::Service,
                runtime_id,
                errors,
            })
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
            Some(Problem::InvalidConfig {
                owner: ConfigOwner::Service,
                runtime_id,
                errors,
            })
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
        let robot = robot_declaring("optional")?;

        let outcome = run_check_with_context(
            &[],
            &sources,
            CheckGraphContext {
                robot: Some(&robot),
            },
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
        let robot = robot_declaring("required")?;

        let outcome = run_check_with_context(
            &[],
            &sources,
            CheckGraphContext {
                robot: Some(&robot),
            },
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
            Some(Problem::InvalidConfig {
                owner: ConfigOwner::Service,
                runtime_id,
                errors,
            })
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

        let [
            Problem::InvalidConfig {
                owner: ConfigOwner::Service,
                runtime_id,
                errors,
            },
        ] = outcome.report.problems.as_slice()
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

    /// The two ways one OFFICIAL service reaches the check engine.
    #[derive(Clone, Copy, Debug)]
    enum ServiceArrival {
        /// The ordinary case: a catalog package, fetched from the staged
        /// binary, carrying no component instances.
        RegistryRef,
        /// A workspace `services/<id>` crate overriding the official's source,
        /// built here - so the schema the declared config is judged against is
        /// the override's own.
        PathOverride,
    }

    const SERVICE_ARRIVALS: [ServiceArrival; 2] =
        [ServiceArrival::RegistryRef, ServiceArrival::PathOverride];

    /// Drive the check engine with the official service `safety` arriving the
    /// stated way, against `robot` and the report its binary emits.
    fn check_safety(
        arrival: ServiceArrival,
        robot: &Robot,
        report: &RawParticipantReport,
    ) -> Result<CheckOutcome> {
        match arrival {
            ServiceArrival::RegistryRef => run_check_with_context(
                &[PlatformArtifactRef {
                    name: "safety".to_string(),
                    kind: ArtifactKind::Service,
                    binary_name: "safety".to_string(),
                    instances: Vec::new(),
                }],
                &[],
                CheckGraphContext { robot: Some(robot) },
                |_| Ok(report.clone()),
                |participant| bail!("no source participant is built here ({})", participant.name),
            ),
            ServiceArrival::PathOverride => run_check_with_context(
                &[],
                &[SourceParticipant::official_service(
                    "safety",
                    "safety",
                    PathBuf::from("/fake/project/services/safety"),
                )],
                CheckGraphContext { robot: Some(robot) },
                |image_ref| bail!("no platform artifact is fetched here ({image_ref})"),
                |_| Ok(report.clone()),
            ),
        }
    }

    /// The report a compiled `safety` service binary emits.
    fn raw_safety(config_schema: Value) -> RawParticipantReport {
        RawParticipantReport {
            artifact: RawArtifact {
                kind: "service".to_string(),
                id: "safety".to_string(),
            },
            config_schema: Some(config_schema),
            connection: None,
        }
    }

    fn service_config_problems(outcome: &CheckOutcome) -> Vec<&Problem> {
        outcome
            .report
            .problems
            .iter()
            .filter(|problem| {
                matches!(
                    problem,
                    Problem::InvalidConfig {
                        owner: ConfigOwner::Service,
                        ..
                    }
                )
            })
            .collect()
    }

    /// The schema a `safety` binary that takes a margin embeds.
    fn wants_a_margin() -> Value {
        serde_json::json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "properties": { "margin_m": { "type": "number" } },
            "required": ["margin_m"],
            "additionalProperties": false,
        })
    }

    /// An official service always runs; a `services.<id>` entry for one is how
    /// its config is authored, and that config is judged against the schema
    /// that binary embeds - by whichever of the two arrivals produced it.
    #[test]
    fn a_declared_official_services_config_is_validated_against_its_binarys_schema() -> Result<()> {
        for arrival in SERVICE_ARRIVALS {
            let robot =
                robot_with_service_config("safety", serde_json::json!({ "margin_m": "wide" }))?;
            let outcome = check_safety(arrival, &robot, &raw_safety(wants_a_margin()))?;
            let [
                Problem::InvalidConfig {
                    owner: ConfigOwner::Service,
                    runtime_id,
                    errors,
                },
            ] = service_config_problems(&outcome).as_slice()
            else {
                panic!("{arrival:?}: expected one service config problem: {outcome:?}");
            };
            assert_eq!(runtime_id, "safety");
            assert!(
                errors
                    .iter()
                    .any(|error| error.starts_with("services.safety.config")),
                "{arrival:?}: {errors:?}"
            );
            assert!(
                errors.iter().any(|error| error.contains("margin_m")),
                "{arrival:?}: {errors:?}"
            );

            let robot =
                robot_with_service_config("safety", serde_json::json!({ "margin_m": 0.4 }))?;
            let outcome = check_safety(arrival, &robot, &raw_safety(wants_a_margin()))?;
            assert!(
                outcome.is_ok(),
                "{arrival:?}: a valid margin must pass: {outcome:?}"
            );
        }
        Ok(())
    }

    /// A path-overridden official is judged against the OVERRIDE's schema, not
    /// against whatever the catalog binary would have wanted: the override is
    /// the binary that will read the config.
    #[test]
    fn a_path_overridden_official_is_judged_against_the_overrides_own_schema() -> Result<()> {
        // The fork took a `mode` where the catalog service takes a `margin_m`.
        let robot = robot_with_service_config("safety", serde_json::json!({ "mode": "strict" }))?;
        let takes_a_mode = serde_json::json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "properties": { "mode": { "type": "string" } },
            "required": ["mode"],
            "additionalProperties": false,
        });

        let outcome = check_safety(
            ServiceArrival::PathOverride,
            &robot,
            &raw_safety(takes_a_mode),
        )?;
        assert!(
            outcome.is_ok(),
            "the override's own schema decides: {outcome:?}"
        );

        let outcome = check_safety(
            ServiceArrival::PathOverride,
            &robot,
            &raw_safety(wants_a_margin()),
        )?;
        assert!(
            !service_config_problems(&outcome).is_empty(),
            "a config the built binary rejects must fail: {outcome:?}"
        );
        Ok(())
    }

    /// An official service the document does not declare has nothing authored
    /// to be wrong. It runs either way, so a schema it would have wanted a
    /// config for must not turn every undeclared official into a failure.
    #[test]
    fn an_undeclared_official_service_is_not_config_checked() -> Result<()> {
        // The fixture declares `mission`, never `safety`.
        let robot = fixture_robot()?;
        for arrival in SERVICE_ARRIVALS {
            let outcome = check_safety(arrival, &robot, &raw_safety(wants_a_margin()))?;
            assert!(
                outcome.is_ok(),
                "{arrival:?}: an undeclared official must not be config-checked: {outcome:?}"
            );
        }
        Ok(())
    }

    fn fixture_robot() -> Result<Robot> {
        crate::source::resolver::parse_robot_from_string(LAUNCH_PLAN_FIXTURE_ROBOT)
    }

    /// The report a compiled `ddsm115` driver binary emits.
    fn raw_driver(
        connection: Option<ConnectionKind>,
        config_schema: Option<Value>,
    ) -> RawParticipantReport {
        RawParticipantReport {
            artifact: RawArtifact {
                kind: "driver".to_string(),
                id: "ddsm115".to_string(),
            },
            config_schema,
            connection,
        }
    }

    /// The two ways one component driver reaches the check engine.
    #[derive(Clone, Copy, Debug)]
    enum Arrival {
        /// A registry-materialized artifact, fetched once and keyed per
        /// declaring instance.
        RegistryRef,
        /// A workspace `components/<id>` crate, built from source.
        WorkspaceSource,
    }

    const ARRIVALS: [Arrival; 2] = [Arrival::RegistryRef, Arrival::WorkspaceSource];

    /// Drive the check engine with one `ddsm115` driver bound to `left_drive`,
    /// arriving the stated way. Both shapes carry the same binary and the same
    /// authored block, so any verdict that differs between them is a second
    /// code path, which is exactly what must not exist.
    fn check_left_drive(
        arrival: Arrival,
        robot: &Robot,
        report: &RawParticipantReport,
    ) -> Result<CheckOutcome> {
        match arrival {
            Arrival::RegistryRef => run_check_with_context(
                &[PlatformArtifactRef {
                    name: "ddsm115".to_string(),
                    kind: ArtifactKind::ComponentDriver,
                    binary_name: "ddsm115".to_string(),
                    instances: vec!["left_drive".to_string()],
                }],
                &[],
                CheckGraphContext { robot: Some(robot) },
                |_| Ok(report.clone()),
                |participant| bail!("no source participant is built here ({})", participant.name),
            ),
            Arrival::WorkspaceSource => run_check_with_context(
                &[],
                &[SourceParticipant::component_driver_with_artifact_id(
                    "left_drive",
                    "ddsm115",
                    PathBuf::from("/fake/project/components/ddsm115"),
                )],
                CheckGraphContext { robot: Some(robot) },
                |image_ref| bail!("no platform artifact is fetched here ({image_ref})"),
                |_| Ok(report.clone()),
            ),
        }
    }

    fn driver_config_problems(outcome: &CheckOutcome) -> Vec<&Problem> {
        outcome
            .report
            .problems
            .iter()
            .filter(|problem| {
                matches!(
                    problem,
                    Problem::InvalidConfig {
                        owner: ConfigOwner::ComponentDriver,
                        ..
                    }
                )
            })
            .collect()
    }

    /// The authored `driver.config` is the driver binary's own configuration
    /// and is validated against the schema that binary embeds, under the
    /// authored path an operator edits.
    #[test]
    fn a_driver_config_is_validated_against_the_binarys_own_schema() -> Result<()> {
        let robot = fixture_robot()?;
        let accepts_reduction = serde_json::json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "properties": { "reduction": { "type": "integer" } },
            "required": ["reduction"],
            "additionalProperties": false,
        });
        let wants_a_port = serde_json::json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "properties": { "port": { "type": "string" } },
            "required": ["port"],
            "additionalProperties": false,
        });

        for arrival in ARRIVALS {
            let outcome = check_left_drive(
                arrival,
                &robot,
                &raw_driver(None, Some(accepts_reduction.clone())),
            )?;
            assert!(
                outcome.is_ok(),
                "{arrival:?}: the authored block satisfies its schema: {outcome:?}"
            );

            let outcome = check_left_drive(
                arrival,
                &robot,
                &raw_driver(None, Some(wants_a_port.clone())),
            )?;
            let [
                Problem::InvalidConfig {
                    owner: ConfigOwner::ComponentDriver,
                    runtime_id,
                    errors,
                },
            ] = driver_config_problems(&outcome).as_slice()
            else {
                panic!("{arrival:?}: expected one driver config problem: {outcome:?}");
            };
            assert_eq!(runtime_id, "left_drive");
            assert!(
                errors
                    .iter()
                    .any(|error| error.starts_with("components.left_drive.driver.config")),
                "{arrival:?}: {errors:?}"
            );
            assert!(
                errors.iter().any(|error| error.contains("port")),
                "{arrival:?}: {errors:?}"
            );
        }
        Ok(())
    }

    /// A driver whose `Config` is `()` embeds the unit schema, and an authored
    /// `config:` under its instance is exactly as wrong as an authored config
    /// on a no-config service. There is no exemption for drivers.
    #[test]
    fn a_configless_driver_rejects_an_authored_config_block() -> Result<()> {
        let robot = fixture_robot()?;
        let unit = serde_json::json!({"type": "null"});
        for arrival in ARRIVALS {
            let outcome = check_left_drive(arrival, &robot, &raw_driver(None, Some(unit.clone())))?;
            assert!(
                !driver_config_problems(&outcome).is_empty(),
                "{arrival:?}: a configless driver must reject an authored config: {outcome:?}"
            );

            // `right_drive` authors no `config:` at all, which is `null` - the
            // same rule a no-config service follows.
            let outcome = run_check_with_context(
                &[PlatformArtifactRef {
                    name: "ddsm115".to_string(),
                    kind: ArtifactKind::ComponentDriver,
                    binary_name: "ddsm115".to_string(),
                    instances: vec!["right_drive".to_string()],
                }],
                &[],
                CheckGraphContext {
                    robot: Some(&robot),
                },
                |_| Ok(raw_driver(None, Some(unit.clone()))),
                |participant| bail!("no source participant is built here ({})", participant.name),
            )?;
            assert!(outcome.is_ok(), "{outcome:?}");
        }
        Ok(())
    }

    /// A driver states the one connection kind it accepts; an instance that
    /// authors another one dies at startup before its bus opens, so the check
    /// engine has to reject the pairing here instead.
    #[test]
    fn an_authored_connection_kind_the_driver_rejects_is_a_problem() -> Result<()> {
        let robot = fixture_robot()?;
        for arrival in ARRIVALS {
            let outcome = check_left_drive(
                arrival,
                &robot,
                &raw_driver(Some(ConnectionKind::Serial), None),
            )?;
            let [
                Problem::ConnectionKindMismatch {
                    instance,
                    artifact_id,
                    declared,
                    authored,
                },
            ] = outcome.report.problems.as_slice()
            else {
                panic!("{arrival:?}: expected one connection problem: {outcome:?}");
            };
            assert_eq!(instance, "left_drive");
            assert_eq!(artifact_id, "ddsm115");
            assert_eq!(*declared, ConnectionKind::Serial);
            assert_eq!(*authored, ConnectionKind::Can);

            // The kind the fixture authors, and a driver that declared none
            // and therefore accepts whatever the document says.
            for accepted in [Some(ConnectionKind::Can), None] {
                let outcome = check_left_drive(arrival, &robot, &raw_driver(accepted, None))?;
                assert!(outcome.is_ok(), "{arrival:?}/{accepted:?}: {outcome:?}");
            }
        }
        Ok(())
    }

    /// Resolution produced a driver for an instance the document does not
    /// declare one for. That is the two halves disagreeing, not an authoring
    /// mistake, so it fails hard rather than passing quietly.
    #[test]
    fn a_resolved_driver_for_an_undriven_instance_is_an_internal_inconsistency() -> Result<()> {
        let mut robot = fixture_robot()?;
        robot
            .robot
            .components
            .get_mut("left_drive")
            .expect("the fixture instance")
            .driver = None;

        for arrival in ARRIVALS {
            let error = check_left_drive(arrival, &robot, &raw_driver(None, None))
                .expect_err("a driver with no authored block must fail hard");
            let message = format!("{error:#}");
            assert!(message.contains("left_drive"), "{arrival:?}: {message}");
            assert!(message.contains("ddsm115"), "{arrival:?}: {message}");
            assert!(
                message.contains("no driver block"),
                "{arrival:?}: {message}"
            );
        }
        Ok(())
    }
}
