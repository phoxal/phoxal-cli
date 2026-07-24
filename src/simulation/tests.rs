//! Tests for simulation orchestration.

use super::report::{build_dry_run_output, simulator_artifact_lines};
use super::setup::LiveSimulationLocks;
use super::webots::webots_launch_args;
use super::*;
use crate::host_paths::test_support::ScratchPhoxalHome;
use crate::resolver::host_target_triple;
use crate::supervisor::ProjectLock;
use crate::webots_stage_root;
use anyhow::Result;
use phoxal::check as graph_check;
use phoxal_api::v0_2::simulation::RobotSpawn;
use phoxal_cli_core::check::source::SourceParticipant;
use phoxal_cli_core::project::launch_plan::{
    CheckedRobotLaunchInput, LaunchMode, LaunchPlan, PlanContext, ROBOT_TOOL_DEVICE,
    ROBOT_TOOL_JOYPAD, SIMULATOR_CONTROLLER_ARTIFACT_NAME, SIMULATOR_SUPERVISOR_ARTIFACT_NAME,
    SIMULATOR_SUPERVISOR_PROVIDER_ID, build_launch_plan, simulator_controller_provider_id,
};
use phoxal_cli_core::project::resolver::{
    ResolvedComponent, ResolvedComponentSource, ResolvedPathOverride, ResolvedPathOverrideKind,
    ResolvedPlatformRuntime, ResolvedRobot, ResolvedTool, ResolvedUserRuntime,
};
use phoxal_cli_core::project::suite::{
    ArtifactKind, fixture_contract_for_tests, fixture_suite_for_tests, fixture_tool_entry_for_tests,
};
use std::fs;
use std::path::{Path, PathBuf};

/// A `LaunchMode::Webots` for tests that only care about exercising the
/// Webots-mode participant/ownership rules, not the world path itself.
fn webots_mode_for_tests() -> LaunchMode {
    LaunchMode::Webots {
        world: PathBuf::from("worlds/test.wbt"),
    }
}

#[test]
fn webots_launch_starts_realtime_batch_mode() {
    assert_eq!(
        webots_launch_args(Path::new("/tmp/staged.wbt")),
        vec!["--mode=realtime", "--batch", "/tmp/staged.wbt"]
    );
}

#[test]
fn live_simulation_locks_remain_held_until_the_setup_owner_drops() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let simulator_lock = temp.path().join("simulator.lock");
    let identity = crate::supervisor::ProjectLockIdentity {
        project: temp.path().join("project"),
        entry: temp.path().join("project/robot.yaml"),
        operation: crate::supervisor::ProjectOperation::Run,
        pid: std::process::id(),
    };
    let locks = LiveSimulationLocks::acquire(&simulator_lock, identity.clone())?;
    assert!(
        ProjectLock::acquire_path(&simulator_lock, identity.clone()).is_err(),
        "the simulator lock must remain held after setup returns"
    );

    drop(locks);
    ProjectLock::acquire_path(&simulator_lock, identity)?;
    Ok(())
}

#[test]
fn spawn_responder_wire_shape_matches_additive_simulation_contract() {
    assert_eq!(
        <SpawnRequest as ContractBody>::TOPIC,
        format!(
            "{}/simulation/spawn",
            <SpawnRequest as ContractBody>::VERSION
        )
    );
    let value = serde_json::to_value(SpawnSet {
        revision: 1,
        robots: vec![RobotSpawn {
            robot_id: "robot-a".to_string(),
            node_string: "Robot { name \"robot-a\" }".to_string(),
        }],
    })
    .unwrap();

    assert_eq!(value["revision"], 1);
    assert_eq!(value["robots"][0]["robot_id"], "robot-a");
    assert_eq!(
        value["robots"][0]["node_string"],
        "Robot { name \"robot-a\" }"
    );
    assert!(value.get("spawn").is_none());
    assert!(value["robots"][0].get("name").is_none());
}

#[test]
fn live_resolve_uses_locked_cargo_workspace() -> Result<()> {
    let _phoxal_home = ScratchPhoxalHome::new()?;
    let temp = tempfile::tempdir()?;
    write_robot_project(temp.path())?;

    let resolved = resolve_project(
        temp.path(),
        SimulateOptions {
            world: "test".to_string(),
            suite_source: Some(temp.path().join("suite.json").display().to_string()),
            ..SimulateOptions::default()
        },
        SimulateMode::Live,
    )?;

    assert_eq!(resolved.resolved.train, "0.1.0");
    assert!(resolved.resolved.components.is_empty());
    Ok(())
}

#[test]
fn dry_run_resolve_uses_locked_cargo_workspace() -> Result<()> {
    let _phoxal_home = ScratchPhoxalHome::new()?;
    let temp = tempfile::tempdir()?;
    write_robot_project(temp.path())?;

    let resolved = resolve_project(
        temp.path(),
        SimulateOptions {
            world: "test".to_string(),
            suite_source: Some(temp.path().join("suite.json").display().to_string()),
            ..SimulateOptions::default()
        },
        SimulateMode::DryRun,
    )?;

    assert_eq!(resolved.resolved.train, "0.1.0");
    Ok(())
}

#[test]
fn no_components_sim_plan_matches_run_plan_participants() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let mut resolved = empty_resolved_robot("robot_v1")?;
    add_robot_tools(&mut resolved);
    resolved.platform_runtimes.push(platform_runtime(
        "drive",
        vec![fixture_contract_for_tests("v0.1::drive::Target", "publish")],
    ));
    resolved.user_runtimes.push(ResolvedUserRuntime {
        name: "mission".to_string(),
        path: PathBuf::from("runtimes/mission"),
        source_hash: "hash".to_string(),
    });
    let sources = vec![SourceParticipant::user_service(
        "mission",
        temp.path().join("runtimes/mission"),
    )];
    let checked = vec![
        service_participant("drive", Vec::new()),
        service_participant("mission", Vec::new()),
    ];

    let run_plan = build_launch_plan(
        LaunchMode::Run,
        &[CheckedRobotLaunchInput {
            project_root: temp.path(),
            resolved: &resolved,
            checked_participants: &checked,
            substitutions: &[],
            source_participants: &sources,
        }],
    )?;
    let sim_plan = build_launch_plan(
        webots_mode_for_tests(),
        &[CheckedRobotLaunchInput {
            project_root: temp.path(),
            resolved: &resolved,
            checked_participants: &checked,
            substitutions: &[],
            source_participants: &sources,
        }],
    )?;

    assert_eq!(participant_ids(&sim_plan), participant_ids(&run_plan));
    assert!(sim_plan.robots[0].substitutions.is_empty());
    Ok(())
}

#[test]
fn one_instance_substitution_is_checked_and_rendered() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let resolved = resolved_with_drive_components(&["left_drive"], false)?;
    let controller_id = simulator_controller_provider_id("robot_v1");
    let checked = vec![
        service_participant("drive", vec![motor_command()]),
        driver_participant("ddsm115", "left_drive", vec![motor_command()]),
        simulator_controller_participant(&controller_id, vec![motor_command()]),
    ];
    let substitutions = simulated_component_records(&checked, &controller_id);
    let sim_participants = sim_checked_participants(&checked);
    let report = graph_check::check_graph(&sim_participants);
    assert!(report.is_ok(), "{report:?}");

    let plan = build_launch_plan(
        webots_mode_for_tests(),
        &[CheckedRobotLaunchInput {
            project_root: temp.path(),
            resolved: &resolved,
            checked_participants: &sim_participants,
            substitutions: &substitutions,
            source_participants: &[],
        }],
    )?;

    assert_eq!(
        substitution_lines(&plan),
        vec![format!(
            "component/left_drive/* : satisfied by {controller_id} (webots-controller)"
        )]
    );
    // The controller is a SIMULATION-MANAGED robot launch participant: it
    // appears here for board presence + controllerArgs rendering, but
    // never gets a CLI-spawned process (Part 5).
    assert_eq!(
        participant_ids(&plan),
        vec![
            "drive",
            controller_id.as_str(),
            "tool-bus-robot_v1",
            "tool-device-robot_v1",
            "tool-joypad-robot_v1",
            "tool-log-robot_v1",
            "tool-telemetry-robot_v1",
        ]
    );
    Ok(())
}

#[test]
fn two_identical_instances_get_disjoint_substitution_sets() {
    let controller_id = simulator_controller_provider_id("robot_v1");
    let checked = vec![
        service_participant("drive", vec![motor_command()]),
        driver_participant("ddsm115", "left_drive", vec![motor_command()]),
        driver_participant("ddsm115", "right_drive", vec![motor_command()]),
        simulator_controller_participant(&controller_id, vec![motor_command()]),
    ];
    // Board display only (no checker involved, see module docs): each
    // dropped driver instance gets its own disjoint substitution record.
    let substitutions = simulated_component_records(&checked, &controller_id);
    let instances = substitutions
        .iter()
        .map(|substitution| substitution.component_instance.as_str())
        .collect::<Vec<_>>();
    assert_eq!(instances, vec!["left_drive", "right_drive"]);
}

#[test]
fn supervisor_and_controller_get_distinct_stable_provider_ids() {
    // Part 1 acceptance: the supervisor is world-scoped and stable; the
    // controller is robot-scoped; the two never collide.
    let supervisor_id = simulator_participant_id_for_resolved_artifact(
        SIMULATOR_SUPERVISOR_ARTIFACT_NAME,
        "robot_v1",
    )
    .expect("supervisor artifact name should map to an id");
    let controller_id = simulator_participant_id_for_resolved_artifact(
        SIMULATOR_CONTROLLER_ARTIFACT_NAME,
        "robot_v1",
    )
    .expect("controller artifact name should map to an id");

    assert_eq!(supervisor_id, SIMULATOR_SUPERVISOR_PROVIDER_ID);
    assert_eq!(controller_id, "simulator-webots-controller-robot_v1");
    assert_ne!(supervisor_id, controller_id);

    // The supervisor id is stable across robots (world-scoped); the
    // controller id is not (robot-scoped), so it generalizes to a
    // multi-robot plan without id collisions.
    let supervisor_id_other_robot = simulator_participant_id_for_resolved_artifact(
        SIMULATOR_SUPERVISOR_ARTIFACT_NAME,
        "robot_v2",
    )
    .expect("supervisor artifact name should map to an id");
    let controller_id_other_robot = simulator_participant_id_for_resolved_artifact(
        SIMULATOR_CONTROLLER_ARTIFACT_NAME,
        "robot_v2",
    )
    .expect("controller artifact name should map to an id");
    assert_eq!(supervisor_id, supervisor_id_other_robot);
    assert_ne!(controller_id, controller_id_other_robot);
}

#[test]
fn path_overridden_simulators_use_the_same_provider_ids_as_official() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let mut resolved = resolved_with_drive_components(&[], false)?;
    resolved.simulators.clear();

    let supervisor_path = temp.path().join("framework/simulator/webots-supervisor");
    let controller_path = temp.path().join("framework/simulator/webots-controller");
    let mut supervisor = simulator_runtime(SIMULATOR_SUPERVISOR_ARTIFACT_NAME);
    supervisor.path_override = Some(supervisor_path.clone());
    supervisor.artifact_ref = format!("path:{}", supervisor_path.display());
    let mut controller = simulator_runtime(SIMULATOR_CONTROLLER_ARTIFACT_NAME);
    controller.path_override = Some(controller_path.clone());
    controller.artifact_ref = format!("path:{}", controller_path.display());
    resolved.simulators.extend([supervisor, controller]);
    resolved.path_overrides = vec![
        ResolvedPathOverride {
            key: "simulator-webots-supervisor".to_string(),
            kind: ResolvedPathOverrideKind::Simulator,
            artifact_name: SIMULATOR_SUPERVISOR_ARTIFACT_NAME.to_string(),
            path: supervisor_path.clone(),
        },
        ResolvedPathOverride {
            key: "simulator-webots-controller".to_string(),
            kind: ResolvedPathOverrideKind::Simulator,
            artifact_name: SIMULATOR_CONTROLLER_ARTIFACT_NAME.to_string(),
            path: controller_path.clone(),
        },
    ];

    let supervisor_id = SIMULATOR_SUPERVISOR_PROVIDER_ID.to_string();
    let controller_id = simulator_controller_provider_id("robot_v1");
    let mut checked = vec![
        service_participant("drive", Vec::new()),
        graph_check::ParticipantApis {
            participant_id: SIMULATOR_SUPERVISOR_ARTIFACT_NAME.to_string(),
            artifact_id: SIMULATOR_SUPERVISOR_ARTIFACT_NAME.to_string(),
            participant_kind: graph_check::ParticipantKind::Simulator,
            participant_class: graph_check::ParticipantClass::Checked,
            api_version: "v1".to_string(),
            config_schema: None,
            scope: graph_check::ParticipantScope::Graph,
            contracts: Vec::new(),
        },
        simulator_controller_participant(SIMULATOR_CONTROLLER_ARTIFACT_NAME, Vec::new()),
    ];
    remap_simulator_participant_ids(&mut checked, &resolved.robot.robot.id)?;

    assert!(checked.iter().any(|participant| {
        participant.participant_id == supervisor_id
            && participant.artifact_id == SIMULATOR_SUPERVISOR_ARTIFACT_NAME
    }));
    assert!(checked.iter().any(|participant| {
        participant.participant_id == controller_id
            && participant.artifact_id == SIMULATOR_CONTROLLER_ARTIFACT_NAME
    }));
    let sources = sim_source_participants(temp.path(), &resolved, None)?;
    let plan = build_launch_plan(
        webots_mode_for_tests(),
        &[CheckedRobotLaunchInput {
            project_root: temp.path(),
            resolved: &resolved,
            checked_participants: &checked,
            substitutions: &[],
            source_participants: &sources,
        }],
    )?;

    assert_eq!(
        participant_ids(&plan),
        vec![
            "drive",
            controller_id.as_str(),
            supervisor_id.as_str(),
            "tool-bus-robot_v1",
            "tool-device-robot_v1",
            "tool-joypad-robot_v1",
            "tool-log-robot_v1",
            "tool-telemetry-robot_v1",
        ]
    );
    for id in [&controller_id, &supervisor_id] {
        let participant = plan.robots[0]
            .participants
            .iter()
            .find(|participant| participant.launch.participant_id == *id)
            .expect("simulator participant should be present");
        assert_eq!(
            participant.launch_ownership,
            phoxal_cli_core::project::launch_plan::LaunchOwnership::SimulationManaged
        );
        // A simulator participant - suite or workspace-overridden - resolves to
        // its one canonical `bin/` simulator binary in the source-free plan
        // (#936); the layout no longer distinguishes the two.
        assert!(matches!(
            &participant.execution,
            phoxal_cli_core::project::launch_plan::ParticipantExecution::OfficialArtifact { .. }
        ));
    }

    let artifact_lines = simulator_artifact_lines(&resolved);
    assert_eq!(artifact_lines.len(), 2);
    assert!(
        artifact_lines
            .iter()
            .any(|line| line.contains(&supervisor_id))
    );
    assert!(
        artifact_lines
            .iter()
            .any(|line| line.contains(&controller_id))
    );

    Ok(())
}

#[test]
fn sim_launch_set_matches_checked_robot_participants_without_drivers() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let resolved = resolved_with_drive_components(&["left_drive"], true)?;
    let controller_id = simulator_controller_provider_id("robot_v1");
    let checked = vec![
        service_participant("drive", Vec::new()),
        service_participant("mission", Vec::new()),
        driver_participant("ddsm115", "left_drive", vec![motor_command()]),
        simulator_controller_participant(&controller_id, vec![motor_command()]),
    ];
    let substitutions = simulated_component_records(&checked, &controller_id);
    let sources = vec![SourceParticipant::user_service(
        "mission",
        temp.path().join("runtimes/mission"),
    )];
    let sim_participants = sim_checked_participants(&checked);
    let plan = build_launch_plan(
        webots_mode_for_tests(),
        &[CheckedRobotLaunchInput {
            project_root: temp.path(),
            resolved: &resolved,
            checked_participants: &sim_participants,
            substitutions: &substitutions,
            source_participants: &sources,
        }],
    )?;

    assert_eq!(
        participant_ids(&plan),
        vec![
            "drive",
            "mission",
            controller_id.as_str(),
            "tool-bus-robot_v1",
            "tool-device-robot_v1",
            "tool-joypad-robot_v1",
            "tool-log-robot_v1",
            "tool-telemetry-robot_v1",
        ]
    );
    assert_eq!(
        plan.robots[0]
            .substitutions
            .iter()
            .map(|substitution| substitution.component_instance.as_str())
            .collect::<Vec<_>>(),
        vec!["left_drive"]
    );
    Ok(())
}

#[test]
fn sim_plan_carries_both_supervisor_and_controller_under_distinct_ids() -> Result<()> {
    // Part 1 acceptance (full): a sim launch plan for a robot with >=1
    // component driver has (a) the supervisor present under the
    // world-scoped id, (b) the driver substitution's
    // provider_participant_id is the controller id, and (c) supervisor id
    // != controller id.
    let temp = tempfile::tempdir()?;
    let mut resolved = resolved_with_drive_components(&["left_drive"], false)?;
    resolved
        .simulators
        .push(simulator_runtime(SIMULATOR_SUPERVISOR_ARTIFACT_NAME));
    let supervisor_id = SIMULATOR_SUPERVISOR_PROVIDER_ID.to_string();
    let controller_id = simulator_controller_provider_id("robot_v1");
    let checked = vec![
        service_participant("drive", vec![motor_command()]),
        driver_participant("ddsm115", "left_drive", vec![motor_command()]),
        graph_check::ParticipantApis {
            participant_id: supervisor_id.clone(),
            artifact_id: "webots-supervisor".to_string(),
            participant_kind: graph_check::ParticipantKind::Simulator,
            participant_class: graph_check::ParticipantClass::Checked,
            api_version: "v1".to_string(),
            config_schema: None,
            scope: graph_check::ParticipantScope::Graph,
            contracts: Vec::new(),
        },
        simulator_controller_participant(&controller_id, vec![motor_command()]),
    ];
    let substitutions = simulated_component_records(&checked, &controller_id);
    let sim_participants = sim_checked_participants(&checked);
    let report = graph_check::check_graph(&sim_participants);
    assert!(report.is_ok(), "{report:?}");

    let plan = build_launch_plan(
        webots_mode_for_tests(),
        &[CheckedRobotLaunchInput {
            project_root: temp.path(),
            resolved: &resolved,
            checked_participants: &sim_participants,
            substitutions: &substitutions,
            source_participants: &[],
        }],
    )?;

    // (a) the supervisor is present under the world-scoped id.
    assert!(participant_ids(&plan).contains(&supervisor_id.as_str()));
    // (b) the driver substitution's provider is the controller id.
    assert_eq!(
        plan.robots[0].substitutions[0].provider_participant_id,
        controller_id
    );
    // (c) supervisor id != controller id.
    assert_ne!(supervisor_id, controller_id);
    Ok(())
}

#[test]
fn stage_simulation_for_robot_produces_a_webots_free_staged_world() -> Result<()> {
    // The Part 5 Live-path plumbing (`stage_and_prepare_webots_spec` ->
    // `stage_simulation_for_robot`) end to end, without Webots: given a
    // sim LaunchPlan carrying both the supervisor and controller
    // participants, staging must write a `.wbt` declaring the robot's
    // EXTERNPROTO and the supervisor's static node, with no static robot
    // instance node.
    //
    // Staging lands under `<project>/.phoxal/webots`, so this test selects
    // a scratch project root rather than touching the working project.
    let _phoxal_home = ScratchPhoxalHome::new()?;
    let temp = tempfile::tempdir()?;
    fs::write(
        temp.path().join("structure.urdf"),
        r#"<robot name="testbot"><link name="base_footprint"/><link name="base_link"/><joint name="base_joint" type="fixed"><parent link="base_footprint"/><child link="base_link"/></joint></robot>"#,
    )?;
    fs::create_dir_all(temp.path().join("worlds"))?;
    let world_source_path = temp.path().join("worlds/default.wbt");
    fs::write(
        &world_source_path,
        "#VRML_SIM R2023b utf8\n\nWorldInfo {\n}\n",
    )?;
    write_driver_crate(temp.path(), "ddsm115")?;
    fs::write(
        temp.path().join("components/ddsm115/component.yaml"),
        "schema: component/v0\ncapabilities: {}\n",
    )?;

    let mut resolved = resolved_with_drive_components(&["left_drive"], false)?;
    resolved
        .simulators
        .push(simulator_runtime(SIMULATOR_SUPERVISOR_ARTIFACT_NAME));
    let supervisor_id = SIMULATOR_SUPERVISOR_PROVIDER_ID.to_string();
    let controller_id = simulator_controller_provider_id("robot_v1");
    let checked = vec![
        service_participant("drive", vec![motor_command()]),
        driver_participant("ddsm115", "left_drive", vec![motor_command()]),
        graph_check::ParticipantApis {
            participant_id: supervisor_id.clone(),
            artifact_id: "webots-supervisor".to_string(),
            participant_kind: graph_check::ParticipantKind::Simulator,
            participant_class: graph_check::ParticipantClass::Checked,
            api_version: "v1".to_string(),
            config_schema: None,
            scope: graph_check::ParticipantScope::Graph,
            contracts: Vec::new(),
        },
        simulator_controller_participant(&controller_id, vec![motor_command()]),
    ];
    let substitutions = simulated_component_records(&checked, &controller_id);
    let sim_participants = sim_checked_participants(&checked);
    let report = graph_check::check_graph(&sim_participants);
    assert!(report.is_ok(), "{report:?}");

    let plan = build_launch_plan(
        webots_mode_for_tests(),
        &[CheckedRobotLaunchInput {
            project_root: temp.path(),
            resolved: &resolved,
            checked_participants: &sim_participants,
            substitutions: &substitutions,
            source_participants: &[],
        }],
    )?;

    let staged = stage_simulation_for_robot(temp.path(), &world_source_path, &resolved, &plan)?;

    let staged_text = std::fs::read_to_string(&staged.staged_world_path)?;
    assert!(
        staged_text.contains("EXTERNPROTO"),
        "staged world should declare the robot's EXTERNPROTO:\n{staged_text}"
    );
    assert!(
        staged_text.contains("supervisor TRUE"),
        "staged world should contain the supervisor node:\n{staged_text}"
    );
    assert!(
        staged_text.contains("phoxal-simulator-webots-supervisor"),
        "staged world should name the supervisor controller:\n{staged_text}"
    );
    assert_eq!(
        staged_text.matches("Robot {").count(),
        1,
        "the staged world should contain exactly one root Robot node (the supervisor)"
    );
    assert_eq!(staged.spawn_descriptors.len(), 1);
    assert_eq!(staged.spawn_descriptors[0].robot_id, "robot_v1");
    assert!(
        staged.spawn_descriptors[0]
            .node_string
            .contains("phoxal-simulator-webots-controller")
    );

    Ok(())
}

#[test]
fn stage_simulation_for_robot_writes_world_with_supervisor_and_no_static_robot() -> Result<()> {
    // Part 5 acceptance: the Live path's staging helper produces a real
    // staged .wbt on disk with the supervisor node and no static robot
    // instance node, using the controller/supervisor ParticipantLaunch
    // records already carried by the Sim LaunchPlan.
    let _phoxal_home = ScratchPhoxalHome::new()?;
    let temp = tempfile::tempdir()?;
    fs::write(
        temp.path().join("structure.urdf"),
        r#"<robot name="testbot"><link name="base_footprint"/><link name="base_link"/><joint name="base_joint" type="fixed"><parent link="base_footprint"/><child link="base_link"/></joint></robot>"#,
    )?;
    fs::create_dir_all(temp.path().join("worlds"))?;
    let world_source = temp.path().join("worlds/test.wbt");
    fs::write(&world_source, "#VRML_SIM R2023b utf8\n\nWorldInfo {\n}\n")?;

    let mut resolved = resolved_with_drive_components(&[], false)?;
    resolved
        .simulators
        .push(simulator_runtime(SIMULATOR_SUPERVISOR_ARTIFACT_NAME));
    resolved
        .simulators
        .push(simulator_runtime(SIMULATOR_CONTROLLER_ARTIFACT_NAME));
    let supervisor_id = SIMULATOR_SUPERVISOR_PROVIDER_ID.to_string();
    let controller_id = simulator_controller_provider_id("robot_v1");
    let checked = vec![
        service_participant("drive", vec![motor_command()]),
        graph_check::ParticipantApis {
            participant_id: supervisor_id.clone(),
            artifact_id: "webots-supervisor".to_string(),
            participant_kind: graph_check::ParticipantKind::Simulator,
            participant_class: graph_check::ParticipantClass::Checked,
            api_version: "v1".to_string(),
            config_schema: None,
            scope: graph_check::ParticipantScope::Graph,
            contracts: Vec::new(),
        },
        graph_check::ParticipantApis {
            participant_id: controller_id.clone(),
            artifact_id: "webots-controller".to_string(),
            participant_kind: graph_check::ParticipantKind::Simulator,
            participant_class: graph_check::ParticipantClass::Checked,
            api_version: "v1".to_string(),
            config_schema: None,
            scope: graph_check::ParticipantScope::Graph,
            contracts: Vec::new(),
        },
    ];
    let sim_participants = sim_checked_participants(&checked);

    let plan = build_launch_plan(
        webots_mode_for_tests(),
        &[CheckedRobotLaunchInput {
            project_root: temp.path(),
            resolved: &resolved,
            checked_participants: &sim_participants,
            substitutions: &[],
            source_participants: &[],
        }],
    )?;

    let staged = stage_simulation_for_robot(temp.path(), &world_source, &resolved, &plan)?;

    assert!(staged.staged_world_path.is_file());
    let staged_text = fs::read_to_string(&staged.staged_world_path)?;
    assert!(
        staged_text.contains("supervisor TRUE"),
        "staged world should contain the supervisor node:\n{staged_text}"
    );
    assert!(
        staged_text.contains("phoxal-simulator-webots-supervisor"),
        "staged world should name the supervisor controller:\n{staged_text}"
    );
    assert_eq!(
        staged_text.matches("Robot {").count(),
        1,
        "the staged world should have exactly one root Robot node (the supervisor):\n{staged_text}"
    );
    assert_eq!(staged.controller_launches.len(), 1);
    assert_eq!(staged.controller_launches[0].0, "robot_v1");
    Ok(())
}

#[test]
fn dry_run_output_shows_webots_supervisor_controller_and_ownership() -> Result<()> {
    // Part 6 acceptance: `simulate --dry-run` must show, explicitly, the
    // Webots app as the CLI-managed child, both simulator artifacts
    // (supervisor + controller) with their ids, and each simulator
    // participant's SIMULATION-MANAGED ownership + the staged world path
    // - without staging or launching anything.
    let temp = tempfile::tempdir()?;
    // `resolved_with_drive_components` already registers a controller
    // simulator runtime; add only the supervisor to get exactly one of
    // each.
    let mut resolved = resolved_with_drive_components(&[], false)?;
    resolved
        .simulators
        .push(simulator_runtime(SIMULATOR_SUPERVISOR_ARTIFACT_NAME));
    let supervisor_id = SIMULATOR_SUPERVISOR_PROVIDER_ID.to_string();
    let controller_id = simulator_controller_provider_id("robot_v1");
    let checked = vec![
        service_participant("drive", vec![motor_command()]),
        graph_check::ParticipantApis {
            participant_id: supervisor_id.clone(),
            artifact_id: "webots-supervisor".to_string(),
            participant_kind: graph_check::ParticipantKind::Simulator,
            participant_class: graph_check::ParticipantClass::Checked,
            api_version: "v1".to_string(),
            config_schema: None,
            scope: graph_check::ParticipantScope::Graph,
            contracts: Vec::new(),
        },
        graph_check::ParticipantApis {
            participant_id: controller_id.clone(),
            artifact_id: "webots-controller".to_string(),
            participant_kind: graph_check::ParticipantKind::Simulator,
            participant_class: graph_check::ParticipantClass::Checked,
            api_version: "v1".to_string(),
            config_schema: None,
            scope: graph_check::ParticipantScope::Graph,
            contracts: Vec::new(),
        },
    ];
    let sim_participants = sim_checked_participants(&checked);

    let launch_plan = build_launch_plan(
        webots_mode_for_tests(),
        &[CheckedRobotLaunchInput {
            project_root: temp.path(),
            resolved: &resolved,
            checked_participants: &sim_participants,
            substitutions: &[],
            source_participants: &[],
        }],
    )?;

    let plan = SimPlan {
        plan: launch_plan,
        ctx: PlanContext {
            robot_path: temp.path().join("robot.yaml"),
            project_root: temp.path().to_path_buf(),
            source: Some(phoxal_cli_core::project::launch_plan::PlanSource {
                resolved,
                source_participants: Vec::new(),
            }),
        },
        runtime_store: phoxal_cli_core::session::stores::runtime::RuntimeStore::new(),
    };

    let output = build_dry_run_output(&plan);

    assert_eq!(output.webots_app.app_id, WEBOTS_APP_ID);
    assert_eq!(output.webots_app.launch_ownership, "cli_managed");
    assert!(
        output
            .webots_app
            .intended_staged_world_path
            .to_string_lossy()
            .contains("test.wbt")
    );

    assert_eq!(output.simulator_artifacts.len(), 2);
    assert!(
        output
            .simulator_artifacts
            .iter()
            .any(|line| line.contains("webots-supervisor") && line.contains(&supervisor_id))
    );
    assert!(
        output
            .simulator_artifacts
            .iter()
            .any(|line| line.contains("webots-controller") && line.contains(&controller_id))
    );

    assert_eq!(output.simulation_managed_participants.len(), 2);
    assert!(
        output
            .simulation_managed_participants
            .iter()
            .any(|line| line.contains(&supervisor_id))
    );
    assert!(
        output
            .simulation_managed_participants
            .iter()
            .any(|line| line.contains(&controller_id))
    );

    Ok(())
}

#[test]
fn real_sim_plan_with_component_names_missing_simulator_provider_still_succeeds() -> Result<()> {
    // phoxal 0.28 dropped the substitution-completeness gate entirely
    // (see `phoxal::check` module docs): whether a simulator provider is
    // actually present is a caller/deployment choice, not something the
    // checker judges. So a sim plan with a component driver but no
    // resolved simulator must still succeed - and the board still labels
    // the driver's component as "simulated by" the controller, because
    // that label is a CLI-side display fact computed from the plan, not
    // a checked one.
    let _phoxal_home = ScratchPhoxalHome::new()?;
    let temp = tempfile::tempdir()?;
    write_robot_project_with_component(temp.path())?;
    let suite_path = write_suite_with_robot_tools(temp.path())?;

    let plan = prepare(
        temp.path(),
        SimulateOptions {
            world: "test".to_string(),
            suite_source: Some(suite_path.display().to_string()),
            ..SimulateOptions::default()
        },
    )?;

    assert_eq!(
        plan.plan.robots[0]
            .substitutions
            .iter()
            .map(|substitution| substitution.component_instance.as_str())
            .collect::<Vec<_>>(),
        vec!["left_drive"]
    );
    assert_eq!(
        plan.plan.robots[0].substitutions[0].provider_participant_id,
        "simulator-webots-controller-testbot"
    );
    Ok(())
}

fn write_robot_project(root: &Path) -> Result<()> {
    fs::write(root.join("robot.yaml"), minimal_robot_yaml())?;
    write_suite_with_robot_tools(root)?;
    fs::write(
        root.join("structure.urdf"),
        r#"<robot name="testbot"><link name="base_footprint"/><link name="base_link"/><joint name="base_joint" type="fixed"><parent link="base_footprint"/><child link="base_link"/></joint></robot>"#,
    )?;
    fs::create_dir_all(root.join("worlds"))?;
    fs::write(
        root.join("worlds/test.wbt"),
        "#VRML_SIM R2023b utf8\n\nWorldInfo {\n}\n",
    )?;
    write_cargo_workspace(root)?;
    Ok(())
}

fn minimal_robot_yaml() -> &'static str {
    r#"schema: robot/v0

robot:
  id: testbot
  namespace: test
  motion_limits:
    max_linear_speed_mps: 0.6
    max_angular_speed_radps: 2.0
  structure: structure.urdf
  kinematic:
    kind: differential
    left_actuators: [left_drive.motor]
    right_actuators: [right_drive.motor]
    left_encoders: [left_drive.encoder]
    right_encoders: [right_drive.encoder]
    wheel_radius_m: 0.1
    wheel_base_m: 0.5
  components: {}

"#
}

fn write_robot_project_with_component(root: &Path) -> Result<()> {
    fs::write(root.join("robot.yaml"), robot_yaml_with_component())?;
    write_suite_with_robot_tools(root)?;
    write_driver_crate(root, "ddsm115")?;
    fs::write(
        root.join("structure.urdf"),
        r#"<robot name="testbot"><link name="base_footprint"/><link name="base_link"/><joint name="base_joint" type="fixed"><parent link="base_footprint"/><child link="base_link"/></joint></robot>"#,
    )?;
    fs::create_dir_all(root.join("worlds"))?;
    fs::write(
        root.join("worlds/test.wbt"),
        "#VRML_SIM R2023b utf8\n\nWorldInfo {\n}\n",
    )?;
    write_cargo_workspace(root)?;
    Ok(())
}

fn robot_yaml_with_component() -> &'static str {
    r#"schema: robot/v0

robot:
  id: testbot
  namespace: test
  motion_limits:
    max_linear_speed_mps: 0.6
    max_angular_speed_radps: 2.0
  structure: structure.urdf
  kinematic:
    kind: omnidirectional
    actuators: [left_drive.motor]
    encoders: []
  components:
    left_drive:
      component: ddsm115
      mount_link: left_wheel
      parameters:
        motor:
          kind: motor
      driver:
        connection: { type: can, bus: 0, node_id: 1 }

"#
}

fn write_suite_with_robot_tools(root: &Path) -> Result<PathBuf> {
    let path = root.join("suite.json");
    let suite = fixture_suite_for_tests(
        ["router", "bus", "joypad", "log", "telemetry", "device"]
            .into_iter()
            .map(|name| {
                fixture_tool_entry_for_tests(name, "0.1.0", &host_target_triple(), true, Vec::new())
            })
            .collect(),
    );
    fs::write(&path, serde_json::to_string_pretty(&suite)?)?;
    Ok(path)
}

/// Writes a fake component-driver crate whose binary carries a
/// hand-rolled participant-metadata linker section - not by
/// depending on `phoxal`/`phoxal-macros` (too heavy for a per-test throwaway
/// crate), but by placing the exact same JSON bytes
/// `phoxal-macros` would emit directly under a manually-attributed
/// `#[link_section]` static. `build_emit_apis_from_source` extracts
/// contracts straight from the built binary's object-file section (the
/// `emit-apis` runtime subcommand this used to fake via stdout is gone),
/// so the fixture fakes the section instead of the subprocess output.
fn write_driver_crate(root: &Path, name: &str) -> Result<()> {
    let dir = root.join("components").join(name);
    fs::create_dir_all(dir.join("src"))?;
    fs::write(
        dir.join("Cargo.toml"),
        format!("[package]\nname = \"driver-{name}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n"),
    )?;
    let json = r#"{"participant_api":"Api","contracts":[{"role":"subscribe","version":"v0.1","contract":"component::MotorCommand","external":false}],"config_schema":{"type":"null"}}"#;
    let len = json.len();
    fs::write(
        dir.join("src/main.rs"),
        format!(
            "#[used]\n#[cfg_attr(target_os = \"macos\", unsafe(link_section = \"__DATA,__phoxal_meta\"))]\n#[cfg_attr(not(target_os = \"macos\"), unsafe(link_section = \".phoxal_api_meta\"))]\nstatic PHOXAL_API_META: [u8; {len}] = *b{json:?};\n\nfn main() {{}}\n"
        ),
    )?;
    fs::write(
        dir.join("component.yaml"),
        "schema: component/v0\nstructure: structure.urdf\ncapabilities:\n  motor:\n    kind: motor\n    target: { kind: joint, id: wheel }\n    command: velocity\n    gear_ratio: 1.0\n",
    )?;
    fs::write(
        dir.join("structure.urdf"),
        "<robot name=\"component\"><link name=\"wheel\"/></robot>\n",
    )?;
    Ok(())
}

fn write_cargo_workspace(root: &Path) -> Result<()> {
    fs::create_dir_all(root.join("src"))?;
    fs::create_dir_all(root.join("train/phoxal/src"))?;
    let mut members = vec!["\".\"".to_string()];
    let components = root.join("components");
    if components.is_dir() {
        for entry in fs::read_dir(components)? {
            let entry = entry?;
            if entry.path().join("Cargo.toml").is_file() {
                members.push(format!(
                    "\"components/{}\"",
                    entry.file_name().to_string_lossy()
                ));
            }
        }
    }
    members.sort();
    fs::write(
        root.join("Cargo.toml"),
        format!(
            r#"[package]
name = "robot-train-anchor"
version = "0.1.0"
edition = "2024"
publish = false

[workspace]
members = [{}]
resolver = "2"

[dependencies]
phoxal = {{ path = "train/phoxal" }}
"#,
            members.join(", ")
        ),
    )?;
    fs::write(root.join("src/lib.rs"), "")?;
    fs::write(
        root.join("train/phoxal/Cargo.toml"),
        "[package]\nname = \"phoxal\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    )?;
    fs::write(root.join("train/phoxal/src/lib.rs"), "")?;
    let status = std::process::Command::new("cargo")
        .arg("generate-lockfile")
        .current_dir(root)
        .status()?;
    anyhow::ensure!(status.success(), "failed to generate fixture Cargo.lock");
    Ok(())
}

fn participant_ids(plan: &LaunchPlan) -> Vec<&str> {
    plan.robots[0]
        .participants
        .iter()
        .map(|participant| participant.launch.participant_id.as_str())
        .collect()
}

fn service_participant(
    id: &str,
    contracts: Vec<graph_check::Contract>,
) -> graph_check::ParticipantApis {
    graph_check::ParticipantApis {
        participant_id: id.to_string(),
        artifact_id: id.to_string(),
        participant_kind: graph_check::ParticipantKind::Service,
        participant_class: graph_check::ParticipantClass::Checked,
        api_version: "v1".to_string(),
        config_schema: None,
        scope: graph_check::ParticipantScope::Graph,
        contracts,
    }
}

fn driver_participant(
    artifact_id: &str,
    instance: &str,
    contracts: Vec<graph_check::Contract>,
) -> graph_check::ParticipantApis {
    graph_check::ParticipantApis {
        participant_id: instance.to_string(),
        artifact_id: artifact_id.to_string(),
        participant_kind: graph_check::ParticipantKind::Driver,
        participant_class: graph_check::ParticipantClass::Checked,
        api_version: "v1".to_string(),
        config_schema: None,
        scope: graph_check::ParticipantScope::ComponentInstance(instance.to_string()),
        contracts,
    }
}

fn simulator_controller_participant(
    provider_participant_id: &str,
    contracts: Vec<graph_check::Contract>,
) -> graph_check::ParticipantApis {
    graph_check::ParticipantApis {
        participant_id: provider_participant_id.to_string(),
        artifact_id: "webots-controller".to_string(),
        participant_kind: graph_check::ParticipantKind::Simulator,
        participant_class: graph_check::ParticipantClass::Checked,
        api_version: "v1".to_string(),
        config_schema: None,
        scope: graph_check::ParticipantScope::Graph,
        contracts,
    }
}

fn motor_command() -> graph_check::Contract {
    graph_check::Contract {
        family: "component::MotorCommand".to_string(),
    }
}

fn empty_resolved_robot(id: &str) -> Result<ResolvedRobot> {
    let yaml = format!(
        r#"schema: robot/v0
robot:
  id: {id}
  namespace: dev
  motion_limits:
    max_linear_speed_mps: 0.6
    max_angular_speed_radps: 2.0
  structure: structure.urdf
  kinematic:
    kind: omnidirectional
    actuators: []
    encoders: []
  components: {{}}
"#
    );
    let robot = phoxal::model::robot::v0::Robot::parse_from_string(&yaml)?;
    Ok(ResolvedRobot {
        robot,
        train: "0.36.0".to_string(),
        target: host_target_triple(),
        platform_runtimes: Vec::new(),
        simulators: Vec::new(),
        user_runtimes: Vec::new(),
        components: Vec::new(),
        tools: Vec::new(),
        path_overrides: Vec::new(),
    })
}

fn resolved_with_drive_components(instances: &[&str], include_user: bool) -> Result<ResolvedRobot> {
    let mut resolved = empty_resolved_robot("robot_v1")?;
    add_robot_tools(&mut resolved);
    resolved
        .platform_runtimes
        .push(platform_runtime("drive", Vec::new()));
    resolved
        .simulators
        .push(simulator_runtime(SIMULATOR_CONTROLLER_ARTIFACT_NAME));
    if include_user {
        resolved.user_runtimes.push(ResolvedUserRuntime {
            name: "mission".to_string(),
            path: PathBuf::from("runtimes/mission"),
            source_hash: "hash".to_string(),
        });
    }
    for instance in instances {
        resolved.components.push(ResolvedComponent {
            instance: (*instance).to_string(),
            source_name: "ddsm115".to_string(),
            assets: phoxal_cli_core::project::resolver::ResolvedComponentPackage {
                package: "phoxal/component-ddsm115".to_string(),
                kind: phoxal_cli_core::project::suite::ArtifactKind::ComponentAssets,
                source: ResolvedComponentSource::Path {
                    path: PathBuf::from("components/ddsm115"),
                },
                path_override: None,
                suite_runtime: None,
            },
            driver: Some(
                phoxal_cli_core::project::resolver::ResolvedComponentPackage {
                    package: "phoxal/component-ddsm115".to_string(),
                    kind: phoxal_cli_core::project::suite::ArtifactKind::ComponentDriver,
                    source: ResolvedComponentSource::Path {
                        path: PathBuf::from("components/ddsm115"),
                    },
                    path_override: None,
                    suite_runtime: None,
                },
            ),
            has_driver: true,
        });
    }
    Ok(resolved)
}

fn platform_runtime(
    name: &str,
    _contracts: Vec<phoxal_cli_core::project::suite::FixtureContract>,
) -> ResolvedPlatformRuntime {
    ResolvedPlatformRuntime {
        name: name.to_string(),
        package: format!("phoxal/service-{name}"),
        kind: ArtifactKind::Service,
        version: "0.1.0".to_string(),
        artifact_ref: format!("service-{name}:0.1.0-v1-stable-{}", host_target_triple()),
        sha256: None,
        url: None,
        size: None,
        published: false,
        published_triples: Vec::new(),
        path_override: None,
        train: "0.36.0".to_string(),
        target: Some(host_target_triple()),
    }
}

fn simulator_runtime(name: &str) -> ResolvedPlatformRuntime {
    ResolvedPlatformRuntime {
        name: name.to_string(),
        package: format!("phoxal/simulator-{name}"),
        kind: ArtifactKind::Simulator,
        version: "0.1.0".to_string(),
        artifact_ref: format!("simulator-{name}:0.1.0-v1-stable-{}", host_target_triple()),
        sha256: None,
        url: None,
        size: None,
        published: false,
        published_triples: Vec::new(),
        path_override: None,
        train: "0.36.0".to_string(),
        target: Some(host_target_triple()),
    }
}

fn add_robot_tools(resolved: &mut ResolvedRobot) {
    resolved.tools.push(tool("tool-bus"));
    resolved.tools.push(tool(ROBOT_TOOL_JOYPAD));
    resolved.tools.push(tool("tool-log"));
    resolved.tools.push(tool("tool-telemetry"));
    resolved.tools.push(tool(ROBOT_TOOL_DEVICE));
}

fn tool(name: &str) -> ResolvedTool {
    ResolvedTool {
        kind: phoxal_cli_core::project::suite::ArtifactKind::Tool,
        name: name.to_string(),
        package: format!("phoxal/{name}"),
        requested: "0.1.0".to_string(),
        resolved: "0.1.0".to_string(),
        repo: "phoxal/framework".to_string(),
        asset: format!("{name}-0.1.0-{}.tar.gz", host_target_triple()),
        binary_name: name.to_string(),
        sha256: "0".repeat(64),
        url: None,
        size: None,
        published: false,
        path_override: None,
        train: "0.36.0".to_string(),
        target: host_target_triple(),
    }
}

/// Write a minimal, fast-building binary crate at `dir` whose package and
/// `[[bin]]` name is `bin_name` - stands in for the real
/// `phoxal-simulator-webots-{supervisor,controller}` crates without
/// depending on the framework workspace or Webots being installed, so
/// this test runs in CI regardless of host Webots availability.
fn write_fake_simulator_crate(dir: &Path, bin_name: &str) -> Result<()> {
    fs::create_dir_all(dir.join("src"))?;
    fs::write(
        dir.join("Cargo.toml"),
        format!(
            "[package]\nname = \"{bin_name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[[bin]]\nname = \"{bin_name}\"\npath = \"src/main.rs\"\n"
        ),
    )?;
    fs::write(
        dir.join("src/main.rs"),
        "fn main() {\n    println!(\"fake simulator binary\");\n}\n",
    )?;
    // Staging now builds with `cargo build --locked` (#936, finding 7), so the
    // stand-in crate needs a committed lockfile exactly like a real project.
    anyhow::ensure!(
        std::process::Command::new("cargo")
            .arg("generate-lockfile")
            .current_dir(dir)
            .status()?
            .success(),
        "failed to generate fixture Cargo.lock in {}",
        dir.display()
    );
    Ok(())
}

/// Bug 1 regression test: staging must produce a real, executable
/// controller binary at the standard Webots layout
/// (`<project>/.phoxal/webots/controllers/<name>/<name>`) for BOTH
/// the supervisor and the per-robot controller when the simulators are
/// path-overridden (the live-gate case), by actually running `cargo
/// build` against fake stand-in crates - not just asserting a path string
/// was computed. Also covers the copy->symlink change: the staged entry
/// must be a symlink to the built binary, not a copy.
#[test]
fn path_overridden_simulators_are_built_and_staged_as_webots_controllers() -> Result<()> {
    let _phoxal_home = ScratchPhoxalHome::new()?;
    let temp = tempfile::tempdir()?;

    let supervisor_dir = temp.path().join("framework/simulator/webots-supervisor");
    let controller_dir = temp.path().join("framework/simulator/webots-controller");
    write_fake_simulator_crate(&supervisor_dir, "phoxal-simulator-webots-supervisor")?;
    write_fake_simulator_crate(&controller_dir, "phoxal-simulator-webots-controller")?;

    let mut resolved = resolved_with_drive_components(&[], false)?;
    resolved.simulators.clear();
    let mut supervisor = simulator_runtime(SIMULATOR_SUPERVISOR_ARTIFACT_NAME);
    supervisor.path_override = Some(supervisor_dir.clone());
    let mut controller = simulator_runtime(SIMULATOR_CONTROLLER_ARTIFACT_NAME);
    controller.path_override = Some(controller_dir.clone());
    resolved.simulators.extend([supervisor, controller]);

    stage_simulator_controller_binaries(&resolved, &crate::Ui::from_env())?;

    let supervisor_binary =
        webots_stage_root::controller_dir("phoxal-simulator-webots-supervisor")?
            .join("phoxal-simulator-webots-supervisor");
    let controller_binary =
        webots_stage_root::controller_dir("phoxal-simulator-webots-controller")?
            .join("phoxal-simulator-webots-controller");

    for binary in [&supervisor_binary, &controller_binary] {
        assert!(
            binary.is_file(),
            "expected staged controller binary to exist at {}",
            binary.display()
        );
        assert!(
            fs::symlink_metadata(binary)?.file_type().is_symlink(),
            "staged controller binary {} should be a symlink, not a copy",
            binary.display()
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(binary)?.permissions().mode();
            assert!(
                mode & 0o111 != 0,
                "staged controller binary {} must be executable (mode {mode:o})",
                binary.display()
            );
        }
    }

    // Confirm it is genuinely the built binary, not an empty placeholder:
    // running it must succeed and print the fake marker.
    let output = std::process::Command::new(&supervisor_binary).output()?;
    assert!(output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("fake simulator binary"),
        "staged supervisor binary did not run as expected"
    );

    Ok(())
}

/// A suite (non path-overridden) simulator with no native-artifact
/// metadata and nothing in the artifact cache must fail loudly during
/// staging rather than silently leaving the controller unstaged - the
/// exact "generic controller" trap bug 1 exists to close.
#[test]
fn suite_simulator_missing_from_cache_is_a_hard_error() -> Result<()> {
    let _phoxal_home = ScratchPhoxalHome::new()?;
    let mut resolved = resolved_with_drive_components(&[], false)?;
    resolved.simulators.clear();
    resolved
        .simulators
        .push(simulator_runtime(SIMULATOR_SUPERVISOR_ARTIFACT_NAME));

    let error = stage_simulator_controller_binaries(&resolved, &crate::Ui::from_env())
        .expect_err("a suite simulator with no cached binary must error, not silently skip");
    let message = format!("{error:#}");
    assert!(
        message.contains("webots-supervisor"),
        "error should name the simulator that failed to provision: {message}"
    );

    Ok(())
}

/// The staged root must resolve under `<project>/.phoxal/webots`, and each mounted
/// component type's staged `meshes/<component_type>` entry must be a
/// SYMLINK to the component's resolved mesh source directory - not a
/// copy - so the vendored/path-pinned source stays authoritative.
#[test]
fn stage_simulation_uses_project_store_and_symlinks_component_meshes() -> Result<()> {
    let _phoxal_home = ScratchPhoxalHome::new()?;
    let temp = tempfile::tempdir()?;
    fs::write(
        temp.path().join("structure.urdf"),
        r#"<robot name="testbot"><link name="base_footprint"/><link name="base_link"/><joint name="base_joint" type="fixed"><parent link="base_footprint"/><child link="base_link"/></joint></robot>"#,
    )?;
    fs::create_dir_all(temp.path().join("worlds"))?;
    let world_source_path = temp.path().join("worlds/default.wbt");
    fs::write(
        &world_source_path,
        "#VRML_SIM R2023b utf8\n\nWorldInfo {\n}\n",
    )?;
    write_driver_crate(temp.path(), "ddsm115")?;
    fs::write(
        temp.path().join("components/ddsm115/component.yaml"),
        "schema: component/v0\ncapabilities: {}\n",
    )?;
    let component_meshes_dir = temp.path().join("components/ddsm115/meshes");
    fs::create_dir_all(&component_meshes_dir)?;
    fs::write(component_meshes_dir.join("wheel.dae"), b"not a real mesh")?;

    let mut resolved = resolved_with_drive_components(&["left_drive"], false)?;
    resolved
        .simulators
        .push(simulator_runtime(SIMULATOR_SUPERVISOR_ARTIFACT_NAME));
    let supervisor_id = SIMULATOR_SUPERVISOR_PROVIDER_ID.to_string();
    let controller_id = simulator_controller_provider_id("robot_v1");
    let checked = vec![
        service_participant("drive", vec![motor_command()]),
        driver_participant("ddsm115", "left_drive", vec![motor_command()]),
        graph_check::ParticipantApis {
            participant_id: supervisor_id.clone(),
            artifact_id: "webots-supervisor".to_string(),
            participant_kind: graph_check::ParticipantKind::Simulator,
            participant_class: graph_check::ParticipantClass::Checked,
            api_version: "v1".to_string(),
            config_schema: None,
            scope: graph_check::ParticipantScope::Graph,
            contracts: Vec::new(),
        },
        simulator_controller_participant(&controller_id, vec![motor_command()]),
    ];
    let substitutions = simulated_component_records(&checked, &controller_id);
    let sim_participants = sim_checked_participants(&checked);
    let report = graph_check::check_graph(&sim_participants);
    assert!(report.is_ok(), "{report:?}");

    let plan = build_launch_plan(
        webots_mode_for_tests(),
        &[CheckedRobotLaunchInput {
            project_root: temp.path(),
            resolved: &resolved,
            checked_participants: &sim_participants,
            substitutions: &substitutions,
            source_participants: &[],
        }],
    )?;

    let staged = stage_simulation_for_robot(temp.path(), &world_source_path, &resolved, &plan)?;

    let root = webots_stage_root::root()?;
    assert!(
        root.ends_with(".phoxal/webots"),
        "staged root should resolve under project-local .phoxal/webots, got {}",
        root.display()
    );
    assert!(
        staged.staged_world_path.starts_with(&root),
        "staged world {} should live under the staged root {}",
        staged.staged_world_path.display(),
        root.display()
    );

    let staged_component_meshes = root.join("meshes").join("ddsm115");
    let meta = fs::symlink_metadata(&staged_component_meshes)?;
    assert!(
        meta.file_type().is_symlink(),
        "staged component meshes {} should be a symlink, not a copy",
        staged_component_meshes.display()
    );
    let link_target = fs::read_link(&staged_component_meshes)?;
    assert_eq!(link_target, component_meshes_dir);
    assert!(
        link_target.is_absolute(),
        "mesh symlink target must be absolute: {}",
        link_target.display()
    );
    assert_eq!(
        fs::read(staged_component_meshes.join("wheel.dae"))?,
        b"not a real mesh"
    );

    Ok(())
}

/// The wipe-per-play guarantee: a previous play's stale staged content
/// must never linger into the next `simulate` invocation - it is one
/// Webots world per run, so a clean slate every play is correct.
#[test]
fn stage_simulation_for_robot_wipes_previous_play_before_restaging() -> Result<()> {
    let _phoxal_home = ScratchPhoxalHome::new()?;
    let temp = tempfile::tempdir()?;
    fs::write(
        temp.path().join("structure.urdf"),
        r#"<robot name="testbot"><link name="base_footprint"/><link name="base_link"/><joint name="base_joint" type="fixed"><parent link="base_footprint"/><child link="base_link"/></joint></robot>"#,
    )?;
    fs::create_dir_all(temp.path().join("worlds"))?;
    let world_source = temp.path().join("worlds/test.wbt");
    fs::write(&world_source, "#VRML_SIM R2023b utf8\n\nWorldInfo {\n}\n")?;

    let mut resolved = resolved_with_drive_components(&[], false)?;
    resolved
        .simulators
        .push(simulator_runtime(SIMULATOR_SUPERVISOR_ARTIFACT_NAME));
    resolved
        .simulators
        .push(simulator_runtime(SIMULATOR_CONTROLLER_ARTIFACT_NAME));
    let supervisor_id = SIMULATOR_SUPERVISOR_PROVIDER_ID.to_string();
    let controller_id = simulator_controller_provider_id("robot_v1");
    let checked = vec![
        service_participant("drive", vec![motor_command()]),
        graph_check::ParticipantApis {
            participant_id: supervisor_id.clone(),
            artifact_id: "webots-supervisor".to_string(),
            participant_kind: graph_check::ParticipantKind::Simulator,
            participant_class: graph_check::ParticipantClass::Checked,
            api_version: "v1".to_string(),
            config_schema: None,
            scope: graph_check::ParticipantScope::Graph,
            contracts: Vec::new(),
        },
        graph_check::ParticipantApis {
            participant_id: controller_id.clone(),
            artifact_id: "webots-controller".to_string(),
            participant_kind: graph_check::ParticipantKind::Simulator,
            participant_class: graph_check::ParticipantClass::Checked,
            api_version: "v1".to_string(),
            config_schema: None,
            scope: graph_check::ParticipantScope::Graph,
            contracts: Vec::new(),
        },
    ];
    let sim_participants = sim_checked_participants(&checked);
    let plan = build_launch_plan(
        webots_mode_for_tests(),
        &[CheckedRobotLaunchInput {
            project_root: temp.path(),
            resolved: &resolved,
            checked_participants: &sim_participants,
            substitutions: &[],
            source_participants: &[],
        }],
    )?;

    let first = stage_simulation_for_robot(temp.path(), &world_source, &resolved, &plan)?;
    assert!(first.staged_world_path.is_file());

    // Plant a stray marker directly under the staged root, simulating
    // leftover content from a previous play that must not survive the
    // next one's staging.
    let root = webots_stage_root::root()?;
    let marker = root.join("stale-from-previous-play.txt");
    fs::write(&marker, b"stale")?;
    assert!(marker.is_file());

    let second = stage_simulation_for_robot(temp.path(), &world_source, &resolved, &plan)?;
    assert!(second.staged_world_path.is_file());
    assert!(
        !marker.exists(),
        "a second stage must wipe the staged root before restaging, but {} survived",
        marker.display()
    );

    Ok(())
}
