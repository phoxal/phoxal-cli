//! The shared "validate through the loader without supervising" entry (#936).
//!
//! The core [`RuntimeLayout::construct_plan`] derives the immutable launch plan
//! from a staged runtime layout, plus the two validation inputs the bin crate
//! owns the validators for: a config-schema pairing per user service (checked
//! with the jsonschema validator) and a contract surface per checked
//! participant (checked with the API-coherence pass). This module is the thin
//! glue that runs those two validators over the constructor's output and
//! returns the plan.
//!
//! It performs no supervisor, board, or socket construction: it is exactly the
//! "validate the staged layout without running it" step `phoxal build` archives
//! behind, and the same plan the universal `run`/`start` will supervise from.
//! Staging (Cargo, the vendored artifact store, `extends:` flattening) already
//! ran; this only reads the compiled layout.

use anyhow::{Result, bail};
use phoxal::check::Problem;
use phoxal_cli_core::project::launch_plan::{LaunchMode, LaunchPlan};
use phoxal_cli_core::project::layout::{LayoutInspection, PlanOptions, RuntimeLayout};
use std::path::Path;

/// Construct and validate the launch plan for a staged runtime layout at
/// `root`, without supervising it. `options` carries the driver policy (#936):
/// an excluded driver is never required, resolved, inspected, or planned, so a
/// driven robot runs on a host whose driver binaries it cannot inspect once
/// `--drivers off` is passed. `inspection` selects the architecture the selected
/// binaries are checked against - the host for an in-place run/start, or a
/// declared `--target` for a `phoxal build` cross bundle. Fails when a user
/// service's compiled config does not match the schema embedded in its binary,
/// or when the checked participants' API contracts are incoherent. Returns the
/// immutable plan the supervisor would launch from.
pub fn validate_layout_plan(
    root: &Path,
    mode: &LaunchMode,
    options: &PlanOptions,
    inspection: LayoutInspection,
) -> Result<LaunchPlan> {
    let constructed =
        RuntimeLayout::construct_plan_with_inspection(root, mode, options, inspection)?;
    // The compiled `robot.yaml` carries each user service's authored config; the
    // constructor pairs it with the schema from the service's binary. Validate
    // through the same jsonschema check the graph pipeline uses.
    let layout = RuntimeLayout::open(root)?;
    let robot = layout.robot();
    let mut config_problems = Vec::new();
    for pairing in &constructed.user_service_configs {
        if let Some(problem) = crate::check::validate_user_service_config(
            &pairing.service_id,
            Some(&pairing.config_schema),
            Some(robot),
        ) {
            config_problems.push(problem);
        }
    }
    if !config_problems.is_empty() {
        bail!(
            "compiled robot.yaml has invalid user-service config:{}",
            format_config_problems(&config_problems)
        );
    }

    // Per-contract API coherence over the checked participants, fed from the
    // binaries' embedded metadata. The constructor already keyed each surface by
    // its plan participant id, so the coherence pass's in-set filter matches.
    let robot_id = constructed
        .plan
        .robots
        .first()
        .map(|robot| robot.id.clone())
        .unwrap_or_default();
    let graph = crate::check::robot_contract_surfaces(&robot_id, &constructed.contract_surfaces);
    let coherence = crate::check::coherence_for_launch_plan(&constructed.plan, &[graph])?;
    crate::check::enforce_coherence(crate::check::CoherenceVerb::Run, &coherence)?;

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
    use phoxal_cli_core::project::layout::{
        DriverSelection, PlanOptions, RequiredRuntimeKind, RuntimeProfile,
    };
    use std::fs;
    use std::path::PathBuf;

    const ROBOT_YAML: &str = r#"schema: robot/v0
robot:
  id: robot_v1
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

    fn synthesize_binary_for(arch: object::Architecture, payload: &[u8]) -> Vec<u8> {
        use object::write::Object;
        let mut obj = Object::new(object::BinaryFormat::Elf, arch, object::Endianness::Little);
        let section = obj.add_section(
            Vec::new(),
            b".phoxal_api_meta".to_vec(),
            object::SectionKind::ReadOnlyData,
        );
        obj.append_section_data(section, payload, 1);
        obj.write().expect("synthesize object file")
    }

    /// Stage a complete Native layout: `robot.yaml` plus a host-architecture
    /// binary under every required `bin/` name, with `mission` carrying the
    /// given config schema.
    fn stage_layout(root: &PathBuf, mission_schema: &str) -> anyhow::Result<()> {
        stage_layout_for(
            root,
            mission_schema,
            phoxal_cli_core::check::participant_metadata::host_architecture(),
        )
    }

    /// [`stage_layout`], synthesizing every binary for `arch` so a cross-target
    /// bundle can be staged and inspected on any host.
    fn stage_layout_for(
        root: &PathBuf,
        mission_schema: &str,
        arch: object::Architecture,
    ) -> anyhow::Result<()> {
        fs::create_dir_all(root)?;
        fs::write(root.join("robot.yaml"), ROBOT_YAML)?;
        let bin = root.join("bin");
        fs::create_dir_all(&bin)?;
        let layout = RuntimeLayout::open(root)?;
        for required in layout.required_runtimes(RuntimeProfile::Native, &DriverSelection::All) {
            if required.kind == RequiredRuntimeKind::Infrastructure {
                continue;
            }
            let payload = if required.binary_name == "mission" {
                format!(
                    r#"{{"participant_api":"Api","contracts":[],"config_schema":{mission_schema}}}"#
                )
            } else {
                r#"{"participant_api":"()","contracts":[],"config_schema":{"type":"null"}}"#
                    .to_string()
            };
            fs::write(
                bin.join(&required.binary_name),
                synthesize_binary_for(arch, payload.as_bytes()),
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
            &LaunchMode::Run,
            &PlanOptions::default(),
            LayoutInspection::Host,
        )?;
        assert_eq!(plan.robots[0].id, "robot_v1");
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
            &LaunchMode::Run,
            &PlanOptions::default(),
            LayoutInspection::Host,
        )
        .expect_err("an invalid config must fail validation")
        .to_string();
        assert!(error.contains("mission"), "{error}");
        assert!(error.contains("invalid user-service config"), "{error}");
        Ok(())
    }

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

    /// `phoxal build --target` inspects a cross bundle against its *declared*
    /// target architecture, so a correct foreign-arch layout validates even
    /// though it will never execute on this host.
    #[test]
    fn target_inspection_accepts_a_declared_foreign_arch() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let root = dir.path().join("build");
        let foreign = foreign_arch();
        stage_layout_for(
            &root,
            r#"{"type":"object","properties":{"speed":{"type":"integer"}}}"#,
            foreign,
        )?;
        let plan = validate_layout_plan(
            &root,
            &LaunchMode::Run,
            &PlanOptions::default(),
            LayoutInspection::Target(foreign),
        )?;
        assert_eq!(plan.robots[0].id, "robot_v1");
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
            foreign,
        )?;
        // Declaring the host architecture for a foreign-built layout must fail:
        // the binaries are the wrong arch for that declared target.
        let error = validate_layout_plan(
            &root,
            &LaunchMode::Run,
            &PlanOptions::default(),
            LayoutInspection::Target(
                phoxal_cli_core::check::participant_metadata::host_architecture(),
            ),
        )
        .expect_err("a wrong-arch binary for the declared target must fail");
        // The precise arch diagnostic lives in the error's source chain.
        let error = format!("{error:#}");
        assert!(error.contains("built for"), "{error}");
        // And the default host inspection likewise rejects a foreign bundle.
        let host_error = validate_layout_plan(
            &root,
            &LaunchMode::Run,
            &PlanOptions::default(),
            LayoutInspection::Host,
        )
        .expect_err("host inspection must reject a foreign bundle");
        let host_error = format!("{host_error:#}");
        assert!(host_error.contains("built for"), "{host_error}");
        Ok(())
    }
}
