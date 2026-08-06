//! The shared "validate through the loader without supervising" entry (#936).
//!
//! The core [`RuntimeLayout::construct_plan`] derives the immutable launch plan
//! from a staged runtime layout, plus the validation input this project crate
//! owns: a config-schema pairing per compiler-owned runtime config (checked
//! with the jsonschema validator). This module is the thin glue that
//! runs that validator over the constructor's output and returns the plan.
//!
//! It performs no supervisor, board, or socket construction: it is exactly the
//! "validate the staged layout without running it" step `phoxal build` archives
//! behind, and the same plan the universal `run`/`start` will supervise from.
//! Staging (`cargo install` materialization, `extends:` flattening) already
//! ran; this only reads the compiled layout.

use anyhow::{Result, bail};
use phoxal_cli_core::check::Problem;
use phoxal_cli_core::project::launch_plan::LaunchPlan;
use phoxal_cli_core::project::launch_plan::RunIdentity;
use phoxal_cli_core::project::layout::{LayoutInspection, PlanOptions, RuntimeLayout};
use std::path::Path;

/// Construct and validate the launch plan for a staged runtime layout at
/// `root`, without supervising it. `options` carries the driver policy (#936):
/// an excluded driver is never required, resolved, inspected, or planned, so a
/// driven robot runs on a host whose driver binaries it cannot inspect once
/// `--drivers off` is passed. `inspection` selects the architecture the selected
/// binaries are checked against - the host for an in-place run/start, or a
/// declared `--target` for a `phoxal build` cross bundle. Fails when compiled
/// runtime config does not match the schema embedded in its binary. Returns
/// the immutable plan the supervisor would launch from.
pub fn validate_layout_plan(
    root: &Path,
    options: &PlanOptions,
    inspection: LayoutInspection,
    run: RunIdentity,
) -> Result<LaunchPlan> {
    let constructed =
        RuntimeLayout::construct_plan_with_inspection(root, options, inspection, run)?;
    // The constructor pairs each compiler-owned config with the schema from
    // its selected binary; validate the carried value directly.
    let mut config_problems = Vec::new();
    for pairing in &constructed.user_runtime_configs {
        if let Some(problem) = crate::validation::validate_user_runtime_config(
            &pairing.runtime_id,
            Some(&pairing.config_schema),
            pairing.config.as_ref(),
            pairing.family,
        ) {
            config_problems.push(problem);
        }
    }
    if !config_problems.is_empty() {
        bail!(
            "compiled participant declarations have invalid runtime config:{}",
            format_config_problems(&config_problems)
        );
    }

    Ok(constructed.plan)
}

fn format_config_problems(problems: &[Problem]) -> String {
    let mut message = String::new();
    for problem in problems {
        let Problem::InvalidConfig { runtime_id, errors } = problem;
        for error in errors {
            message.push_str("\n  - ");
            message.push_str(runtime_id);
            message.push_str(": ");
            message.push_str(error);
        }
    }
    message
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Context;
    use phoxal_cli_core::project::layout::{DriverSelection, PlanOptions, RequiredRuntimeKind};
    use std::fs;
    use std::path::Path;

    const ROBOT_YAML: &str = r#"schema: robot/v0
robot:
  id: testbot
  namespace: dev
  motion_limits:
    max_linear_speed_mps: 0.6
    max_angular_speed_radps: 2.0
  kinematic:
    kind: omnidirectional
    actuators: []
    encoders: []
  components: {}
services:
  mission:
    config:
      speed: 1
"#;

    fn synthesize_binary_as(
        format: object::BinaryFormat,
        arch: object::Architecture,
        payload: &[u8],
    ) -> Vec<u8> {
        use object::write::Object;
        let (segment, name): (&[u8], &[u8]) = match format {
            object::BinaryFormat::MachO => (b"__DATA", b"__phoxal_meta"),
            _ => (b"", b".phoxal_meta"),
        };
        let mut obj = Object::new(format, arch, object::Endianness::Little);
        let section = obj.add_section(
            segment.to_vec(),
            name.to_vec(),
            object::SectionKind::ReadOnlyData,
        );
        obj.append_section_data(section, payload, 1);
        obj.write().expect("synthesize object file")
    }

    fn metadata(id: &str, kind: &str, schema: &str) -> anyhow::Result<Vec<u8>> {
        let schema: serde_json::Value = serde_json::from_str(schema)?;
        Ok(serde_json::to_vec(&serde_json::json!({
            "schema": phoxal_runtime_contract::PARTICIPANT_METADATA_SCHEMA,
            "id": id,
            "kind": kind,
            "config_schema": schema,
        }))?)
    }

    fn required_kind(kind: RequiredRuntimeKind) -> &'static str {
        match kind {
            RequiredRuntimeKind::Brain => "brain",
            RequiredRuntimeKind::OfficialService | RequiredRuntimeKind::UserService => "service",
            RequiredRuntimeKind::ComponentDriver => "driver",
        }
    }

    /// Stage a complete Native layout: canonical `robot.json` plus a host-architecture
    /// binary under every required `bin/` name, with `mission` carrying the
    /// given config schema.
    fn stage_layout(root: &Path, mission_schema: &str) -> anyhow::Result<()> {
        stage_layout_for(
            root,
            mission_schema,
            phoxal_cli_core::check::participant_metadata::host_binary_format(),
            phoxal_cli_core::check::participant_metadata::host_architecture(),
        )
    }

    /// [`stage_layout`], synthesizing every binary as `format`/`arch` so both a
    /// host layout and a declared-cross-target (ELF) bundle can be staged and
    /// inspected on any host.
    fn stage_layout_for(
        root: &Path,
        mission_schema: &str,
        format: object::BinaryFormat,
        arch: object::Architecture,
    ) -> anyhow::Result<()> {
        crate::stage::write_test_layout(root, ROBOT_YAML)?;
        let bin = root.join("bin");
        let layout = RuntimeLayout::open(root)?;
        for required in layout.required_runtimes(&DriverSelection::All) {
            let payload = if required.binary_name == "mission" {
                metadata("mission", "service", mission_schema)?
            } else {
                metadata(
                    &required.identity,
                    required_kind(required.kind),
                    r#"{"type":"null"}"#,
                )?
            };
            fs::write(
                bin.join(&required.binary_name),
                synthesize_binary_as(format, arch, &payload),
            )?;
        }
        Ok(())
    }

    #[test]
    fn a_valid_layout_validates_and_returns_its_plan() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let root = dir.path().join("build");
        // The mission config `{speed: 1}` satisfies this schema.
        stage_layout(
            &root,
            r#"{"type":"object","properties":{"speed":{"type":"integer"}}}"#,
        )?;
        let plan = validate_layout_plan(
            &root,
            &PlanOptions::default(),
            LayoutInspection::Host,
            RunIdentity::default(),
        )?;
        assert_eq!(plan.robots[0].id, "testbot");
        assert!(
            plan.robots[0]
                .participants
                .iter()
                .any(|participant| participant.launch.participant_id == "mission")
        );
        Ok(())
    }

    #[test]
    fn an_invalid_user_service_config_fails_validation() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let root = dir.path().join("build");
        // The mission config `{speed: 1}` is an integer, not the required string.
        stage_layout(
            &root,
            r#"{"type":"object","properties":{"speed":{"type":"string"}},"required":["speed"]}"#,
        )?;
        let error = validate_layout_plan(
            &root,
            &PlanOptions::default(),
            LayoutInspection::Host,
            RunIdentity::default(),
        )
        .expect_err("an invalid config must fail validation")
        .to_string();
        assert!(error.contains("mission"), "{error}");
        assert!(error.contains("invalid runtime config"), "{error}");
        Ok(())
    }

    #[test]
    fn a_declared_user_service_validates_its_own_config() -> anyhow::Result<()> {
        // The service's REAL `services.<id>.config` is validated against the
        // schema its own binary emits (#950 review finding 1).
        let dir = tempfile::tempdir()?;
        let root = dir.path().join("build");
        stage_layout_with_user_service(
            &root,
            r#"{"type":"object","properties":{"speed":{"type":"integer"}},"required":["speed"]}"#,
        )?;
        let plan = validate_layout_plan(
            &root,
            &PlanOptions::default(),
            LayoutInspection::Host,
            RunIdentity::default(),
        )?;
        assert!(
            plan.robots[0]
                .participants
                .iter()
                .any(|participant| participant.launch.participant_id == "mission"),
            "the declared user service is a plan participant"
        );
        Ok(())
    }

    #[test]
    fn an_invalid_user_service_config_fails_naming_the_services_map() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let root = dir.path().join("build");
        // The authored speed is an integer; the schema demands a string.
        stage_layout_with_user_service(
            &root,
            r#"{"type":"object","properties":{"speed":{"type":"string"}},"required":["speed"]}"#,
        )?;
        let error = validate_layout_plan(
            &root,
            &PlanOptions::default(),
            LayoutInspection::Host,
            RunIdentity::default(),
        )
        .expect_err("an invalid service config must fail validation")
        .to_string();
        assert!(error.contains("services.mission.config"), "{error}");
        Ok(())
    }

    #[test]
    fn compiler_driver_wiring_is_not_forwarded_as_runtime_config() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let root = dir.path().join("build");
        stage_layout_with_driver(&root, r#"{"type":"null"}"#)?;
        let plan = validate_layout_plan(
            &root,
            &PlanOptions::default(),
            LayoutInspection::Host,
            RunIdentity::default(),
        )?;
        let driver = plan.robots[0]
            .participants
            .iter()
            .find(|participant| {
                participant.launch.participant_id == "wheel"
                    && matches!(
                        participant.execution,
                        phoxal_cli_core::project::launch_plan::ParticipantExecution::ComponentDriver { .. }
                    )
            })
            .context("wheel driver must be planned")?;
        assert_eq!(
            driver.launch.config, None,
            "compiler-side connection metadata must not be sent to a unit-config driver"
        );
        Ok(())
    }

    /// Stage a layout whose compiled declarations include the `mission` user
    /// service with `config: {speed: 1}` and whose bin/ carries a binary for it
    /// emitting `service_schema` as its config schema.
    fn stage_layout_with_user_service(root: &Path, service_schema: &str) -> anyhow::Result<()> {
        crate::stage::write_test_layout(root, ROBOT_YAML)?;
        let bin = root.join("bin");
        let layout = RuntimeLayout::open(root)?;
        let format = phoxal_cli_core::check::participant_metadata::host_binary_format();
        let arch = phoxal_cli_core::check::participant_metadata::host_architecture();
        for required in layout.required_runtimes(&DriverSelection::All) {
            let payload = match required.binary_name.as_str() {
                "mission" => metadata("mission", "service", service_schema)?,
                _ => metadata(
                    &required.identity,
                    required_kind(required.kind),
                    r#"{"type":"null"}"#,
                )?,
            };
            fs::write(
                bin.join(&required.binary_name),
                synthesize_binary_as(format, arch, &payload),
            )?;
        }
        Ok(())
    }

    fn stage_layout_with_driver(root: &Path, driver_schema: &str) -> anyhow::Result<()> {
        crate::stage::write_test_layout(root, ROBOT_YAML)?;
        let participants_path = root
            .join(phoxal_cli_core::project::layout::ASSETS_DIR)
            .join(phoxal_cli_core::project::layout::PARTICIPANTS_ASSET);
        let mut participants =
            phoxal_cli_core::project::layout::decode_participants(&participants_path)?;
        participants.push(phoxal_manifest::Participant {
            id: "wheel".to_string(),
            kind: phoxal_manifest::ParticipantKind::Driver,
            component_instance: Some("wheel".to_string()),
            config: Some(serde_json::json!({
                "connection": {
                    "type": "serial",
                    "port": "/dev/ttyUSB0",
                    "baud": 115200
                }
            })),
        });
        fs::write(
            &participants_path,
            phoxal_cli_core::project::layout::encode_participants(&participants)?,
        )?;

        let bin = root.join("bin");
        let layout = RuntimeLayout::open(root)?;
        let format = phoxal_cli_core::check::participant_metadata::host_binary_format();
        let arch = phoxal_cli_core::check::participant_metadata::host_architecture();
        for required in layout.required_runtimes(&DriverSelection::All) {
            let payload = match required.kind {
                RequiredRuntimeKind::ComponentDriver => {
                    metadata(&required.identity, "driver", driver_schema)?
                }
                RequiredRuntimeKind::UserService => {
                    metadata(&required.identity, "service", r#"{"type":"object"}"#)?
                }
                _ => metadata(
                    &required.identity,
                    required_kind(required.kind),
                    r#"{"type":"null"}"#,
                )?,
            };
            fs::write(
                bin.join(&required.binary_name),
                synthesize_binary_as(format, arch, &payload),
            )?;
        }
        Ok(())
    }

    use phoxal_cli_core::check::participant_metadata::ExpectedTarget;

    /// A concrete architecture that is never the host's, so a "foreign" layout
    /// can be staged and inspected deterministically on any runner.
    fn foreign_arch() -> object::Architecture {
        if phoxal_cli_core::check::participant_metadata::host_architecture()
            == object::Architecture::X86_64
        {
            object::Architecture::Aarch64
        } else {
            object::Architecture::X86_64
        }
    }

    /// A little-endian ELF [`ExpectedTarget`] for `arch`, standing in for a Linux
    /// cross target's declared signature.
    fn elf_target(arch: object::Architecture) -> ExpectedTarget {
        ExpectedTarget {
            format: object::BinaryFormat::Elf,
            architecture: arch,
            endianness: object::Endianness::Little,
        }
    }

    /// `phoxal build --target` inspects a cross bundle against its *declared*
    /// target signature, so a correct foreign-arch layout validates even
    /// though it will never execute on this host.
    #[test]
    fn target_inspection_accepts_a_declared_foreign_arch() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let root = dir.path().join("build");
        let foreign = foreign_arch();
        stage_layout_for(
            &root,
            r#"{"type":"object","properties":{"speed":{"type":"integer"}}}"#,
            object::BinaryFormat::Elf,
            foreign,
        )?;
        let plan = validate_layout_plan(
            &root,
            &PlanOptions::default(),
            LayoutInspection::Target(elf_target(foreign)),
            RunIdentity::default(),
        )?;
        assert_eq!(plan.robots[0].id, "testbot");
        Ok(())
    }

    /// The declared-target inspection still rejects a binary built for the wrong
    /// architecture for that target - a foreign-arch layout inspected against the
    /// host architecture fails precisely.
    #[test]
    fn target_inspection_rejects_the_wrong_arch_for_the_declared_target() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let root = dir.path().join("build");
        let foreign = foreign_arch();
        stage_layout_for(
            &root,
            r#"{"type":"object","properties":{"speed":{"type":"integer"}}}"#,
            object::BinaryFormat::Elf,
            foreign,
        )?;
        // Declaring the host architecture for a foreign-built layout must fail:
        // the binaries are the wrong arch for that declared target.
        let error = validate_layout_plan(
            &root,
            &PlanOptions::default(),
            LayoutInspection::Target(elf_target(
                phoxal_cli_core::check::participant_metadata::host_architecture(),
            )),
            RunIdentity::default(),
        )
        .expect_err("a wrong-arch binary for the declared target must fail");
        // The precise arch diagnostic lives in the error's source chain.
        let error = format!("{error:#}");
        assert!(error.contains("built for"), "{error}");
        // And the default host inspection likewise rejects a foreign bundle -
        // on a wrong arch (Linux host) or wrong container format (macOS host,
        // whose native format is Mach-O, not the staged ELF).
        let host_error = validate_layout_plan(
            &root,
            &PlanOptions::default(),
            LayoutInspection::Host,
            RunIdentity::default(),
        )
        .expect_err("host inspection must reject a foreign bundle");
        let host_error = format!("{host_error:#}");
        assert!(
            host_error.contains("the selected target expects"),
            "{host_error}"
        );
        Ok(())
    }
}
