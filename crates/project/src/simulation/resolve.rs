//! Project resolution and checked simulation launch-plan construction.

use super::{
    ResolvedSimulation, SimulateOptions, ensure_exactly_one_simulator,
    official_simulator_participants, remap_simulator_participant_ids, sim_checked_participants,
};
use crate::build::cargo::SourceArtifacts;
use crate::resolve::project::resolve_with_train;
use crate::validation::CheckGraphContext;
use crate::validation::build_participant_report_from_binary;
use crate::validation::check_artifact_refs_from_resolved;
use crate::validation::extract_participant_report_from_staged_runtime;
use crate::validation::extract_participant_report_from_staged_tool;
use crate::validation::fetch_participant_report_from_tool;
use crate::validation::run_check_with_context;
use crate::validation::tool_participants_from_resolved;
use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use phoxal_cli_core::project::launch_plan::CheckedRobotLaunchInput;
use phoxal_cli_core::project::launch_plan::LaunchMode;
use phoxal_cli_core::project::launch_plan::LaunchPlan;
use phoxal_cli_core::project::launch_plan::RunIdentity;
use phoxal_cli_core::project::launch_plan::build_launch_plan;
use phoxal_cli_core::project::resolver::BundlePlan;
use phoxal_cli_core::project::resolver::ResolveOptions;
use phoxal_cli_core::simulation::world;
use std::collections::BTreeMap;
use std::path::Path;

pub(crate) fn resolve_project(
    project_start: &Path,
    options: SimulateOptions,
    reporter: &dyn crate::Reporter,
) -> Result<ResolvedSimulation> {
    resolve_project_with(
        project_start,
        options,
        reporter,
        |robot, project_root, options| {
            resolve_with_train(robot, project_root, options, |train| {
                reporter.report(crate::PreparationEvent::ProjectResolved {
                    train: train.to_string(),
                });
            })
        },
    )
}

fn resolve_project_with(
    project_start: &Path,
    options: SimulateOptions,
    reporter: &dyn crate::Reporter,
    resolver: impl FnOnce(
        &phoxal_manifest::source::robot::v0::Manifest,
        &Path,
        ResolveOptions,
    ) -> Result<BundlePlan>,
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
        || {
            resolver(
                &robot,
                &project_root,
                ResolveOptions {
                    offline: options.offline,
                    ..Default::default()
                },
            )
        },
    )?;
    Ok(ResolvedSimulation {
        project_root,
        world_path,
        resolved,
    })
}

#[cfg(test)]
mod resolve_project_tests {
    use super::*;

    struct Reporter;
    impl crate::Reporter for Reporter {
        fn report(&self, _event: crate::PreparationEvent) {}
    }

    #[test]
    fn resolve_project_forwards_offline_to_the_resolution_seam() -> Result<()> {
        let project = tempfile::tempdir()?;
        std::fs::create_dir_all(project.path().join("worlds"))?;
        std::fs::write(
            project.path().join("worlds/test.wbt"),
            "#VRML_SIM R2025a utf8",
        )?;
        std::fs::write(
            project.path().join("robot.yaml"),
            "schema: robot/v0\nrobot:\n  id: test\n  namespace: dev\n  motion_limits:\n    max_linear_speed_mps: 1.0\n    max_angular_speed_radps: 1.0\n  kinematic:\n    kind: omnidirectional\n    actuators: [drive.motor]\n    encoders: []\n  components:\n    drive:\n      component: wheel\n      mount_link: base\n",
        )?;
        let reporter = Reporter;
        let seen = std::sync::atomic::AtomicBool::new(false);
        let error = resolve_project_with(
            project.path(),
            SimulateOptions {
                world: "worlds/test.wbt".to_string(),
                offline: true,
            },
            &reporter,
            |_robot, _root, options| {
                assert!(options.offline, "simulation must never drop --offline");
                seen.store(true, std::sync::atomic::Ordering::SeqCst);
                anyhow::bail!("stop after observing options")
            },
        )
        .expect_err("the test resolver intentionally stops after observing options");
        assert!(seen.load(std::sync::atomic::Ordering::SeqCst), "{error:#}");
        Ok(())
    }
}

/// Build the checked simulation launch plan from the one already-prepared,
/// unpublished candidate. Source metadata reads use compiler-reported batch
/// artifacts; registry metadata reads use that candidate's flat `bin/` store.
pub(crate) struct CheckedSimulationInput<'a> {
    pub(crate) project_root: &'a Path,
    pub(crate) world: &'a Path,
    pub(crate) resolved: &'a BundlePlan,
    pub(crate) candidate_root: &'a Path,
    pub(crate) source_participants: &'a [phoxal_cli_core::check::source::SourceParticipant],
    pub(crate) source_artifacts: &'a SourceArtifacts,
    pub(crate) offline: bool,
    pub(crate) run: RunIdentity,
}

pub(crate) fn build_checked_sim_launch_plan(
    input: CheckedSimulationInput<'_>,
    reporter: &dyn crate::Reporter,
) -> Result<LaunchPlan> {
    let CheckedSimulationInput {
        project_root,
        world,
        resolved,
        candidate_root,
        source_participants,
        source_artifacts,
        offline,
        run,
    } = input;
    let metadata_source_participants = source_participants.to_vec();
    // Webots substitutes physical drivers out of the graph. Apply that
    // command-specific selection before registry materialization and metadata
    // fetching, not after Cargo has already done unnecessary host work.
    let platform_refs = check_artifact_refs_from_resolved(
        resolved,
        phoxal_cli_core::project::layout::DriverSelection::None,
    );
    let tool_participants = tool_participants_from_resolved(resolved)?;
    // Materialize the full selected registry set once into the caller's
    // unpublished candidate. This candidate is later published as the
    // simulation runtime layout; there is no scratch staging tree and no
    // second per-participant materialization pass.
    // A valid Cargo robot shares its normal target directory. Keep metadata
    // checking independent for deliberately incomplete validation fixtures:
    // their later graph error is more useful than failing target discovery.
    let materialize_settings = crate::stage::MaterializeSettings {
        profile: crate::build::materialise::MaterializeProfile::Debug,
        target_dir: crate::build::cargo::cargo_target_dir(project_root, offline).ok(),
    };
    let extra_registry_runtimes = resolved
        .simulators
        .iter()
        .filter(|runtime| runtime.source_path().is_none())
        .collect::<Vec<_>>();
    crate::stage::materialize_candidate_store(
        candidate_root,
        resolved,
        &extra_registry_runtimes,
        offline,
        None,
        &materialize_settings,
        reporter,
        |_crate_dir, name| {
            source_artifacts
                .binary_named(name)
                .map(std::path::PathBuf::from)
        },
    )?;
    let bin_dir = candidate_root.join("bin");
    let official_by_name = resolved
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
            robot: Some(&resolved.source_manifest),
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
            build_participant_report_from_binary(
                participant,
                source_artifacts.binary(participant)?,
                reporter,
            )
        },
    )?;

    let mut checked_participants = metadata_outcome.checked_participants.clone();
    remap_simulator_participant_ids(
        &mut checked_participants,
        &resolved.source_manifest.robot.id,
    )?;
    let official_simulators = official_simulator_participants(candidate_root, resolved)?;
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
            source_participants,
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
            r#"{{"schema":"phoxal/participant-metadata/v0","id":"{SIMULATOR_CONTROLLER_ARTIFACT_NAME}","kind":"simulator","class":"checked","config_schema":{{"type":"null"}}}}"#
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
    /// constructing `BundlePlan` directly), so an empty `components: {}`
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
    /// build` through the selected-source batch - the one leg of
    /// `build_checked_sim_launch_plan` that cannot be swapped for a mock
    /// closure the way `run_check_with_context`'s other unit tests do.
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
        let json = format!(
            r#"{{"schema":"phoxal/participant-metadata/v0","id":"avoid","kind":"service","class":"checked","config_schema":{schema}}}"#
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

    /// A `BundlePlan` carrying exactly one user service (`avoid`, built
    /// from `write_invalid_config_service_fixture`) whose robot.yaml config
    /// (`{"gain": "fast"}`) violates that service's own emitted schema
    /// (`gain` must be a number), plus one PATH-overridden simulator (built
    /// from `write_simulator_fixture`) so `ensure_exactly_one_simulator` is
    /// satisfied without ever touching the network (a registry-resolved
    /// simulator would `cargo install` for real).
    fn bundle_plan_with_invalid_service_config(
        crate_dir: PathBuf,
        simulator_dir: PathBuf,
    ) -> Result<BundlePlan> {
        let mut robot = phoxal_manifest::source::robot::parse_from_string(FIXTURE_ROBOT)?;
        robot
            .services
            .get_mut("avoid")
            .expect("fixture declares the avoid service")
            .config = Some(serde_json::json!({ "gain": "fast" }));

        let target = crate::resolve::project::host_target_triple();
        let compiled = crate::stage::compile_test_bundle(&robot)?;
        Ok(BundlePlan {
            source_manifest: robot,
            compiled,
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
        let resolved = bundle_plan_with_invalid_service_config(crate_dir, simulator_dir)?;
        let source_participants =
            super::super::participants::sim_source_participants(temp.path(), &resolved)?;
        let source_artifacts = crate::build::cargo::build_selected_source_artifacts(
            &source_participants,
            None,
            crate::build::profile::Profile::Debug,
            None,
            false,
            &crate::SilentReporter,
        )?;
        let candidate = crate::stage::begin_runtime_layout(temp.path(), &resolved)?;
        let world = temp.path().join("world.wbt");

        let result = build_checked_sim_launch_plan(
            CheckedSimulationInput {
                project_root: temp.path(),
                world: &world,
                resolved: &resolved,
                candidate_root: candidate.path(),
                source_participants: &source_participants,
                source_artifacts: &source_artifacts,
                offline: false,
                run: RunIdentity::default(),
            },
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
