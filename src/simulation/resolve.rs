//! Project resolution and checked simulation launch-plan construction.

use super::{
    ResolvedSimulation, SimulateOptions, driver_metadata_unavailable, ensure_exactly_one_simulator,
    official_simulator_participants, remap_simulator_participant_ids, sim_checked_participants,
    sim_source_participants,
};
use crate::check::CheckGraphContext;
use crate::check::build_participant_report_from_source;
use crate::check::check_artifact_refs_from_resolved;
use crate::check::extract_participant_report_from_staged_runtime;
use crate::check::extract_participant_report_from_staged_tool;
use crate::check::fetch_participant_report_from_tool;
use crate::check::run_check_with_context;
use crate::check::tool_participants_from_resolved;
use crate::resolver::resolve;
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
use phoxal_cli_core::project::suite::Suite;
use phoxal_cli_core::simulation::world;
use std::collections::BTreeMap;
use std::path::Path;

pub(crate) fn resolve_project(
    project_start: &Path,
    options: SimulateOptions,
) -> Result<ResolvedSimulation> {
    let robot_path = phoxal_cli_core::project::resolver::discover_robot_yaml(project_start)
        .with_context(|| format!("failed to find robot.yaml from {}", project_start.display()))?;
    let project_root = robot_path
        .parent()
        .context("robot.yaml did not have a parent directory")?
        .to_path_buf();
    let world_path = world::resolve_world(&project_root, &options.world)?;
    let robot = phoxal_cli_core::project::resolver::load_robot(&robot_path)?;
    let suite = crate::commands::load_suite_for_robot_from_source(
        options.suite_source.clone(),
        &project_root,
    )?;

    // Resolve Cargo-workspace component drivers for compile-time metadata and
    // for their crate-owned model assets. Physical drivers are never launched.
    let resolved = resolve(
        &robot,
        &project_root,
        suite.as_ref(),
        ResolveOptions {
            ..ResolveOptions::default()
        },
    )?;
    Ok(ResolvedSimulation {
        robot_path,
        project_root,
        world_path,
        resolved,
        suite,
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
    suite: Option<&Suite>,
    run: RunIdentity,
) -> Result<LaunchPlan> {
    let source_participants = sim_source_participants(project_root, resolved, suite)
        .with_context(|| "failed to prepare source participants for simulation metadata")?;
    let metadata_source_participants = source_participants.clone();
    // A Suite-sourced component driver is a platform ref here too (docs
    // #21), exactly like `build`/`run` - synthesized from suite
    // metadata rather than built from source. Only a Path/Git-overridden
    // driver crate reaches the `build` closure below.
    let platform_refs = check_artifact_refs_from_resolved(resolved);
    let tool_participants = tool_participants_from_resolved(resolved)?;
    let mut official_by_ref = resolved
        .platform_runtimes
        .iter()
        .map(|runtime| (runtime.artifact_ref().to_string(), runtime))
        .collect::<BTreeMap<_, _>>();
    official_by_ref.extend(crate::check::component_driver_runtimes_by_ref(resolved));
    let tools_by_ref = resolved
        .tools
        .iter()
        .map(|tool| (tool.asset.clone(), tool))
        .collect::<BTreeMap<_, _>>();

    let metadata_outcome = run_check_with_context(
        &platform_refs,
        &tool_participants,
        &metadata_source_participants,
        CheckGraphContext {
            robot: Some(&resolved.robot),
        },
        |artifact_ref| {
            if let Some(runtime) = official_by_ref.get(artifact_ref) {
                return extract_participant_report_from_staged_runtime(runtime);
            }
            if let Some(tool) = tools_by_ref.get(artifact_ref) {
                return extract_participant_report_from_staged_tool(tool);
            }
            Err(anyhow!(
                "resolved official artifact {artifact_ref} is not in the suite"
            ))
        },
        fetch_participant_report_from_tool,
        |participant| {
            if participant.kind == SourceParticipantKind::ComponentDriver {
                return build_participant_report_from_source(participant)
                    .map_err(|error| driver_metadata_unavailable(participant, error));
            }
            build_participant_report_from_source(participant)
        },
    )?;

    let mut checked_participants = metadata_outcome.checked_participants.clone();
    remap_simulator_participant_ids(&mut checked_participants, &resolved.robot.robot.id)?;
    let official_simulators = official_simulator_participants(resolved)?;
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
    crate::check::ensure_check_outcome_ok(&metadata_outcome)?;

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
    use phoxal::model::robot::v0::Robot;
    use phoxal_cli_core::project::launch_plan::RunIdentity;
    use phoxal_cli_core::project::launch_plan::SIMULATOR_CONTROLLER_ARTIFACT_NAME;
    use phoxal_cli_core::project::resolver::ResolvedPlatformRuntime;
    use phoxal_cli_core::project::resolver::ResolvedUserRuntime;
    use phoxal_cli_core::project::suite::ArtifactKind;
    use std::path::PathBuf;

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
    /// (`gain` must be a number), plus one official simulator (bypassed via
    /// the `#[cfg(test)]` `https://example.invalid/` staging shortcut in
    /// `check::extract_participant_report_from_staged_runtime`, the same
    /// technique `check::tests` already uses) so `ensure_exactly_one_simulator`
    /// is satisfied without touching the network.
    fn resolved_robot_with_invalid_service_config(crate_dir: PathBuf) -> Result<ResolvedRobot> {
        let mut robot = Robot::parse_from_string(FIXTURE_ROBOT)?;
        robot
            .services
            .get_mut("avoid")
            .expect("fixture declares the avoid service")
            .config = Some(serde_json::json!({ "gain": "fast" }));

        let target = crate::resolver::host_target_triple();
        Ok(ResolvedRobot {
            robot,
            train: "0.42.0".to_string(),
            target: target.clone(),
            platform_runtimes: Vec::new(),
            simulators: vec![ResolvedPlatformRuntime {
                name: SIMULATOR_CONTROLLER_ARTIFACT_NAME.to_string(),
                package: "phoxal/simulator-webots-controller".to_string(),
                kind: ArtifactKind::Simulator,
                version: "0.1.0".to_string(),
                artifact_ref: "simulator-webots-controller-v0.1.0.tar.zst".to_string(),
                sha256: None,
                url: Some("https://example.invalid/webots-controller".to_string()),
                size: None,
                published: true,
                published_triples: vec![target.clone()],
                path_override: None,
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
            path_overrides: Vec::new(),
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
        let crate_dir = write_invalid_config_service_fixture(temp.path());
        let resolved = resolved_robot_with_invalid_service_config(crate_dir)?;

        let result = build_checked_sim_launch_plan(
            temp.path(),
            &temp.path().join("world.wbt"),
            &resolved,
            None,
            RunIdentity::default(),
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
