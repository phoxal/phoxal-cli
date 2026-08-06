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
use phoxal_cli_core::project::layout::{LayoutInspection, RuntimeLayout};
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
    inspection: LayoutInspection,
    run: RunIdentity,
) -> Result<LaunchPlan> {
    let constructed = RuntimeLayout::construct_plan_with_inspection(root, inspection, run)?;
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
    use phoxal_cli_core::project::intent::RunIntent;
    use phoxal_cli_core::project::requirements::RequiredParticipantKind;
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
    actuators:
      - wheel.motor
    encoders: []
  components:
    wheel:
      component: wheel
      mount_link: base
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
        Ok(crate::stage::test_metadata_payload(
            id,
            kind,
            serde_json::from_str(schema)?,
        ))
    }

    fn required_kind(kind: RequiredParticipantKind) -> &'static str {
        match kind {
            RequiredParticipantKind::Brain => "brain",
            RequiredParticipantKind::OfficialService | RequiredParticipantKind::UserService => {
                "service"
            }
            RequiredParticipantKind::ComponentDriver => "driver",
            RequiredParticipantKind::WorldClock => "simulator",
        }
    }

    /// Stage a complete finalized bundle whose `bin/` carries one synthesized
    /// binary per required participant, with `mission` declaring
    /// `mission_schema`.
    fn stage_bundle_for(
        root: &Path,
        mission_schema: &str,
        format: object::BinaryFormat,
        arch: object::Architecture,
    ) -> anyhow::Result<()> {
        crate::stage::write_test_bundle(root, ROBOT_YAML, &RunIntent::default(), &[])?;
        let bin = root.join("bin");
        let layout = RuntimeLayout::open(root)?;
        for (binary_name, required) in layout.requirements().selected_binaries() {
            let payload = if binary_name == "mission" {
                metadata("mission", "service", mission_schema)?
            } else {
                metadata(
                    &required.artifact_id,
                    required_kind(required.kind),
                    r#"{"type":"null"}"#,
                )?
            };
            fs::write(
                bin.join(binary_name),
                synthesize_binary_as(format, arch, &payload),
            )?;
        }
        Ok(())
    }

    fn stage_bundle(root: &Path, mission_schema: &str) -> anyhow::Result<()> {
        stage_bundle_for(
            root,
            mission_schema,
            phoxal_cli_core::check::participant_metadata::host_binary_format(),
            phoxal_cli_core::check::participant_metadata::host_architecture(),
        )
    }

    #[test]
    fn a_valid_bundle_validates_and_returns_its_plan() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let root = dir.path().join("build");
        stage_bundle(
            &root,
            r#"{"type":"object","properties":{"speed":{"type":"integer"}}}"#,
        )?;
        let plan = validate_layout_plan(&root, LayoutInspection::Host, RunIdentity::default())?;
        assert_eq!(plan.robots[0].id, "testbot");
        assert!(
            plan.robots[0]
                .participants
                .iter()
                .any(|participant| participant.launch.participant_id == "mission")
        );
        Ok(())
    }

    /// The service's authored `services.<id>.config` is validated against the
    /// schema its own binary embeds.
    #[test]
    fn an_invalid_user_service_config_fails_naming_the_services_map() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let root = dir.path().join("build");
        // The authored speed is an integer; the schema demands a string.
        stage_bundle(
            &root,
            r#"{"type":"object","properties":{"speed":{"type":"string"}},"required":["speed"]}"#,
        )?;
        let error = validate_layout_plan(&root, LayoutInspection::Host, RunIdentity::default())
            .expect_err("an invalid config must fail validation")
            .to_string();
        assert!(error.contains("services.mission.config"), "{error}");
        Ok(())
    }

    use phoxal_cli_core::check::participant_metadata::ExpectedTarget;

    /// A concrete architecture that is never the host's, so a "foreign" bundle
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

    fn elf_target(arch: object::Architecture) -> ExpectedTarget {
        ExpectedTarget {
            format: object::BinaryFormat::Elf,
            architecture: arch,
            endianness: object::Endianness::Little,
        }
    }

    /// `phoxal build --target` inspects a cross bundle against its *declared*
    /// target signature, so a correct foreign-arch bundle validates even though
    /// it will never execute on this host - and the same bundle inspected
    /// against the host, or against the wrong architecture for that target,
    /// still fails precisely.
    #[test]
    fn target_inspection_accepts_the_declared_target_and_rejects_the_wrong_one()
    -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let root = dir.path().join("build");
        let foreign = foreign_arch();
        stage_bundle_for(
            &root,
            r#"{"type":"object","properties":{"speed":{"type":"integer"}}}"#,
            object::BinaryFormat::Elf,
            foreign,
        )?;
        let plan = validate_layout_plan(
            &root,
            LayoutInspection::Target(elf_target(foreign)),
            RunIdentity::default(),
        )?;
        assert_eq!(plan.robots[0].id, "testbot");

        let error = format!(
            "{:#}",
            validate_layout_plan(
                &root,
                LayoutInspection::Target(elf_target(
                    phoxal_cli_core::check::participant_metadata::host_architecture(),
                )),
                RunIdentity::default(),
            )
            .expect_err("a wrong-arch binary for the declared target must fail")
        );
        assert!(error.contains("built for"), "{error}");

        let host_error = format!(
            "{:#}",
            validate_layout_plan(&root, LayoutInspection::Host, RunIdentity::default())
                .expect_err("host inspection must reject a foreign bundle")
        );
        assert!(
            host_error.contains("the selected target expects"),
            "{host_error}"
        );
        Ok(())
    }
}
