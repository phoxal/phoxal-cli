//! Project resolution and checked simulation launch-plan construction.

use super::{
    ResolvedSimulation, SimulateOptions, driver_metadata_unavailable, ensure_exactly_one_simulator,
    official_simulator_participants, remap_simulator_participant_ids, sim_checked_participants,
    sim_source_participants,
};
use crate::resolve::project::resolve;
use crate::validation::CheckGraphContext;
use crate::validation::build_participant_report_from_source;
use crate::validation::check_artifact_refs_from_resolved;
use crate::validation::extract_participant_report_from_staged_runtime;
use crate::validation::extract_participant_report_from_staged_tool;
use crate::validation::fetch_participant_report_from_tool;
use crate::validation::run_check_with_context;
use crate::validation::tool_participants_from_resolved;
use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use phoxal_cli_core::check::source::SourceParticipantKind;
use phoxal_cli_core::project::launch_plan::CheckedRobotLaunchInput;
use phoxal_cli_core::project::launch_plan::LaunchMode;
use phoxal_cli_core::project::launch_plan::LaunchPlan;
use phoxal_cli_core::project::launch_plan::RunIdentity;
use phoxal_cli_core::project::launch_plan::build_launch_plan;
use phoxal_cli_core::project::resolver::ResolveOptions;
use phoxal_cli_core::project::resolver::ResolvedRobot;
use phoxal_cli_core::simulation::world;
use std::collections::BTreeMap;
use std::path::Path;

pub(crate) fn resolve_project(
    project_start: &Path,
    options: SimulateOptions,
    reporter: &dyn crate::Reporter,
) -> Result<ResolvedSimulation> {
    let robot_path = phoxal_cli_core::project::resolver::discover_robot_yaml(project_start)
        .with_context(|| format!("failed to find robot.yaml from {}", project_start.display()))?;
    let project_root = robot_path
        .parent()
        .context("robot.yaml did not have a parent directory")?
        .to_path_buf();
    let world_path = world::resolve_world(&project_root, &options.world)?;
    let robot = phoxal_cli_core::project::resolver::load_robot(&robot_path)?;

    // Resolve Cargo-workspace component drivers for compile-time metadata and
    // for their crate-owned model assets. Physical drivers are never launched.
    let resolved = crate::progress::run_phase(
        reporter,
        crate::PhaseId::new("validate"),
        "Validating robot.yaml",
        || resolve(&robot, &project_root, ResolveOptions::default()),
    )?;
    Ok(ResolvedSimulation {
        project_root,
        world_path,
        resolved,
    })
}

/// Build the checked simulation launch plan. Every source participant
/// (drivers, path-overridden services/simulators) rebuilds live - there is no
/// disk cache for metadata extraction (`check::build_participant_report_from_source`
/// never caches).
pub(crate) fn build_checked_sim_launch_plan(
    project_root: &Path,
    world: &Path,
    resolved: &ResolvedRobot,
    offline: bool,
    run: RunIdentity,
    reporter: &dyn crate::Reporter,
) -> Result<LaunchPlan> {
    let source_participants = sim_source_participants(project_root, resolved)
        .with_context(|| "failed to prepare source participants for simulation metadata")?;
    let metadata_source_participants = source_participants.clone();
    // A registry-sourced component driver is a platform ref here too (docs
    // #21), exactly like `build`/`run` - materialized via `cargo install`
    // rather than built from source. Only a Path/Git-overridden driver crate
    // reaches the `build` closure below.
    let platform_refs = check_artifact_refs_from_resolved(resolved);
    let tool_participants = tool_participants_from_resolved(resolved)?;
    // Materialize every official service, tool, and registry component
    // driver this check needs metadata from, up front - the same
    // `cargo install` path `run`/`build` use. This is a metadata read, not
    // staging: it materializes into a SCRATCH candidate that is never
    // published, never the live `.phoxal/bundle/` - the real staging pass
    // (`live_simulate_setup`) runs later and owns publishing. Touching the
    // live bundle here would violate the stager's atomicity promise for
    // every prior run still using it while this check runs.
    let scratch = crate::stage::begin_runtime_layout(project_root, resolved)
        .context("failed to stage a scratch layout for simulation metadata")?;
    // A valid Cargo robot shares its normal target directory. Keep metadata
    // checking independent for deliberately incomplete validation fixtures:
    // their later graph error is more useful than failing target discovery.
    let materialize_settings = crate::stage::MaterializeSettings {
        profile: crate::build::materialise::MaterializeProfile::Debug,
        target_dir: crate::build::cargo::cargo_target_dir(project_root, offline).ok(),
    };
    crate::stage::materialize_official_store(
        scratch.path(),
        resolved,
        offline,
        None,
        &materialize_settings,
        reporter,
        |crate_dir, name| {
            crate::build::cargo::build_source_binary(crate_dir, name, reporter, None, offline)
        },
    )?;
    for runtime in crate::validation::component_driver_runtimes_by_ref(resolved).values() {
        crate::stage::materialize_component_driver(
            scratch.path(),
            runtime,
            offline,
            None,
            &materialize_settings,
            reporter,
        )?;
    }
    let bin_dir = scratch.path().join("bin");
    let mut official_by_name = resolved
        .platform_runtimes
        .iter()
        .map(|runtime| {
            (
                phoxal_cli_core::project::resolver::official_binary_name(
                    runtime.kind,
                    &runtime.name,
                ),
                runtime,
            )
        })
        .collect::<BTreeMap<_, _>>();
    official_by_name.extend(crate::validation::component_driver_runtimes_by_ref(
        resolved,
    ));
    let tools_by_name = resolved
        .tools
        .iter()
        .map(|tool| (tool.binary_name.clone(), tool))
        .collect::<BTreeMap<_, _>>();

    let metadata_outcome = run_check_with_context(
        &platform_refs,
        &tool_participants,
        &metadata_source_participants,
        CheckGraphContext {
            robot: Some(&resolved.robot),
        },
        |binary_name| {
            if let Some(runtime) = official_by_name.get(binary_name) {
                return extract_participant_report_from_staged_runtime(&bin_dir, runtime);
            }
            if let Some(tool) = tools_by_name.get(binary_name) {
                return extract_participant_report_from_staged_tool(&bin_dir, tool);
            }
            Err(anyhow!(
                "resolved official artifact {binary_name} was not materialized into bin/"
            ))
        },
        fetch_participant_report_from_tool,
        |participant| {
            if participant.kind == SourceParticipantKind::ComponentDriver {
                return build_participant_report_from_source(participant, offline, reporter)
                    .map_err(|error| driver_metadata_unavailable(participant, error));
            }
            build_participant_report_from_source(participant, offline, reporter)
        },
    )?;

    let mut checked_participants = metadata_outcome.checked_participants.clone();
    remap_simulator_participant_ids(&mut checked_participants, &resolved.robot.robot.id)?;
    let official_simulators =
        official_simulator_participants(project_root, resolved, offline, reporter)?;
    checked_participants.extend(official_simulators);
    let sim_participants = sim_checked_participants(&checked_participants);
    // The complete simulation surface is the only place this can be asked:
    // the controller is validated here and then handed to Webots rather than
    // entering the resident launch plan the ordinary graph check sees.
    ensure_exactly_one_simulator(&sim_participants)?;
    // Reject exactly what `build`/`run` reject: `metadata_outcome` already
    // carries the real config-schema validation (`InvalidConfig` problems)
    // and any official artifact that could not be obtained
    // (`missing_images`) from `run_check_with_context` above. Validate it
    // through the same shared gate, `ensure_check_outcome_ok`, that
    // `run::prepare::run_source_check` also uses, rather than a locally
    // reconstructed, always-empty report (organization: `phoxal simulate`
    // silently accepted invalid user config and missing images).
    crate::validation::ensure_check_outcome_ok(&metadata_outcome)?;

    let plan = build_launch_plan(
        LaunchMode::Webots {
            world: world.to_path_buf(),
        },
        &[CheckedRobotLaunchInput {
            project_root,
            resolved,
            checked_participants: &sim_participants,
            source_participants: &source_participants,
        }],
        run,
    )?;
    Ok(plan)
}

#[cfg(test)]
mod tests {
    use super::*;
    use phoxal_cli_core::project::catalog::ArtifactKind;
    use phoxal_cli_core::project::launch_plan::RunIdentity;
    use phoxal_cli_core::project::launch_plan::SIMULATOR_CONTROLLER_ARTIFACT_NAME;
    use phoxal_cli_core::project::resolver::ResolvedPathOverride;
    use phoxal_cli_core::project::resolver::ResolvedPathOverrideKind;
    use phoxal_cli_core::project::resolver::ResolvedPlatformRuntime;
    use phoxal_cli_core::project::resolver::ResolvedUserRuntime;
    use std::path::PathBuf;

    /// A minimal Webots-controller fixture crate: a real `cargo build`-able
    /// binary carrying a hand-written participant metadata linker section,
    /// exactly like `write_invalid_config_service_fixture`. Used as a PATH
    /// -overridden simulator so `official_simulator_participants` never
    /// touches the network in tests (a registry-resolved simulator would).
    fn write_simulator_fixture(dir: &Path) -> PathBuf {
        let crate_dir = dir.join(SIMULATOR_CONTROLLER_ARTIFACT_NAME);
        std::fs::create_dir_all(crate_dir.join("src")).expect("create fixture crate dirs");
        std::fs::write(
            crate_dir.join("Cargo.toml"),
            format!(
                "[package]\nname = \"fixture-{SIMULATOR_CONTROLLER_ARTIFACT_NAME}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[[bin]]\nname = \"phoxal-simulator-{SIMULATOR_CONTROLLER_ARTIFACT_NAME}\"\npath = \"src/main.rs\"\n"
            ),
        )
        .expect("write fixture Cargo.toml");
        std::fs::write(
            crate_dir.join("Cargo.lock"),
            format!(
                "version = 4\n\n[[package]]\nname = \"fixture-{SIMULATOR_CONTROLLER_ARTIFACT_NAME}\"\nversion = \"0.1.0\"\n"
            ),
        )
        .expect("write fixture Cargo.lock");
        let json = format!(
            r#"{{"id":"{SIMULATOR_CONTROLLER_ARTIFACT_NAME}","config_schema":{{"type":"null"}}}}"#
        );
        let escaped = json.replace('\\', "\\\\").replace('"', "\\\"");
        let len = json.len();
        std::fs::write(
            crate_dir.join("src/main.rs"),
            format!(
                "#[used]\n\
                 #[cfg_attr(target_os = \"macos\", unsafe(link_section = \"__DATA,__phoxal_meta\"))]\n\
                 #[cfg_attr(not(target_os = \"macos\"), unsafe(link_section = \".phoxal_meta\"))]\n\
                 static PHOXAL_META: [u8; {len}] = *b\"{escaped}\";\n\n\
                 fn main() {{}}\n"
            ),
        )
        .expect("write fixture main.rs");
        crate_dir
    }

    /// A minimal single-service robot. Cross-references between `kinematic`
    /// and `components` are never validated by `Robot::parse_from_string`
    /// itself (that happens inside `resolve()`, which this test bypasses by
    /// constructing `ResolvedRobot` directly), so an empty `components: {}`
    /// is fine here.
    const FIXTURE_ROBOT: &str = r#"schema: robot/v0
robot:
  id: testbot
  namespace: test
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
  components: {}
services:
  avoid: {}
"#;

    /// Writes a standalone, dependency-free Cargo binary crate at
    /// `dir/avoid` whose compiled binary carries a real participant metadata
    /// linker section (`.phoxal_meta` / `__DATA,__phoxal_meta`, matching
    /// `phoxal-macros`' `link_section_attrs`) declaring a config schema that
    /// requires a numeric `gain`. Hand-writing the section instead of
    /// depending on the real `phoxal` framework crate keeps the fixture free
    /// of registry/network dependencies while still exercising a REAL `cargo
    /// build` through `build_participant_report_from_source` - the one leg of
    /// `build_checked_sim_launch_plan` that cannot be swapped for a mock
    /// closure the way `run_check_with_context`'s other unit tests do,
    /// because `build_checked_sim_launch_plan` does not take an injectable
    /// build closure.
    fn write_invalid_config_service_fixture(dir: &Path) -> PathBuf {
        let crate_dir = dir.join("avoid");
        std::fs::create_dir_all(crate_dir.join("src")).expect("create fixture crate dirs");
        std::fs::write(
            crate_dir.join("Cargo.toml"),
            "[package]\nname = \"avoid\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        )
        .expect("write fixture Cargo.toml");
        std::fs::write(
            crate_dir.join("Cargo.lock"),
            "version = 4\n\n[[package]]\nname = \"avoid\"\nversion = \"0.1.0\"\n",
        )
        .expect("write fixture Cargo.lock");

        let schema = r#"{"type":"object","required":["gain"],"properties":{"gain":{"type":"number"}},"additionalProperties":false}"#;
        let json = format!(r#"{{"id":"avoid","config_schema":{schema}}}"#);
        let escaped = json.replace('\\', "\\\\").replace('"', "\\\"");
        let len = json.len();
        std::fs::write(
            crate_dir.join("src/main.rs"),
            format!(
                "#[used]\n\
                 #[cfg_attr(target_os = \"macos\", unsafe(link_section = \"__DATA,__phoxal_meta\"))]\n\
                 #[cfg_attr(not(target_os = \"macos\"), unsafe(link_section = \".phoxal_meta\"))]\n\
                 static PHOXAL_META: [u8; {len}] = *b\"{escaped}\";\n\n\
                 fn main() {{}}\n"
            ),
        )
        .expect("write fixture main.rs");
        crate_dir
    }

    /// A `ResolvedRobot` carrying exactly one user service (`avoid`, built
    /// from `write_invalid_config_service_fixture`) whose robot.yaml config
    /// (`{"gain": "fast"}`) violates that service's own emitted schema
    /// (`gain` must be a number), plus one PATH-overridden simulator (built
    /// from `write_simulator_fixture`) so `ensure_exactly_one_simulator` is
    /// satisfied without ever touching the network (a registry-resolved
    /// simulator would `cargo install` for real).
    fn resolved_robot_with_invalid_service_config(
        crate_dir: PathBuf,
        simulator_dir: PathBuf,
    ) -> Result<ResolvedRobot> {
        let mut robot = phoxal::model::source::robot::parse_from_string(FIXTURE_ROBOT)?;
        robot
            .services
            .get_mut("avoid")
            .expect("fixture declares the avoid service")
            .config = Some(serde_json::json!({ "gain": "fast" }));

        let target = crate::resolve::project::host_target_triple();
        Ok(ResolvedRobot {
            robot,
            train: "0.42.0".to_string(),
            target: target.clone(),
            platform_runtimes: Vec::new(),
            simulators: vec![ResolvedPlatformRuntime {
                name: SIMULATOR_CONTROLLER_ARTIFACT_NAME.to_string(),
                package: "phoxal/simulator-webots-controller".to_string(),
                kind: ArtifactKind::Simulator,
                path_override: Some(simulator_dir.clone()),
                train: "0.42.0".to_string(),
                target: Some(target),
            }],
            user_runtimes: vec![ResolvedUserRuntime {
                name: "avoid".to_string(),
                path: crate_dir,
                source_hash: "fixture".to_string(),
            }],
            user_tools: Vec::new(),
            undeclared_runtimes: Vec::new(),
            components: Vec::new(),
            tools: Vec::new(),
            path_overrides: vec![ResolvedPathOverride {
                key: "phoxal/simulator-webots-controller".to_string(),
                kind: ResolvedPathOverrideKind::Simulator,
                artifact_name: SIMULATOR_CONTROLLER_ARTIFACT_NAME.to_string(),
                path: simulator_dir,
            }],
        })
    }

    /// Reproduces the organization-tracked defect: `phoxal simulate` silently
    /// accepted a robot whose `services.<id>.config` violates that
    /// participant's own compiled-in config schema, while `phoxal build` and
    /// `phoxal run` both correctly reject it. Drives a REAL fixture crate
    /// through the actual simulate resolution path end to end (a genuine
    /// `cargo build` of a participant binary, then
    /// `build_checked_sim_launch_plan` itself) rather than asserting against
    /// a re-implemented copy of its validation logic.
    #[test]
    fn rejects_a_user_service_whose_config_violates_its_own_schema() -> Result<()> {
        let temp = tempfile::tempdir()?;
        // `begin_runtime_layout` (organization#951 WS4 review) genuinely
        // stages the candidate now, including the robot structure file the
        // manifest declares - it must exist for staging to reach the config
        // validation this test actually exercises.
        std::fs::write(temp.path().join("structure.urdf"), "<robot/>")?;
        let crate_dir = write_invalid_config_service_fixture(temp.path());
        let simulator_dir = write_simulator_fixture(temp.path());
        let resolved = resolved_robot_with_invalid_service_config(crate_dir, simulator_dir)?;

        let result = build_checked_sim_launch_plan(
            temp.path(),
            &temp.path().join("world.wbt"),
            &resolved,
            false,
            RunIdentity::default(),
            &crate::SilentReporter,
        );

        let error = result.expect_err(
            "a user service configured against its own emitted schema must be rejected by \
             simulate, exactly like `phoxal build`/`phoxal run`",
        );
        let message = error.to_string();
        assert!(
            message.contains("avoid") && message.contains("gain"),
            "{message}"
        );
        Ok(())
    }
}
