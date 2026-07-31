//! Static Webots world staging.
//!
//! `simulation::webots::proto` is a real, tested render engine (robot PROTO
//! generation, instance-node rendering, world staging) with no caller before
//! this module. This module wires it into the `simulate` launch flow: given
//! the resolved robot + the base world + each robot's controller launch
//! inputs, it stages a copy of the world that:
//!
//! - Declares an `EXTERNPROTO` for each robot's generated PROTO.
//! - Adds exactly one static robot instance.
//! - Assigns that robot the `phoxal-simulator-webots-controller` controller.
//! - Passes only stable robot, namespace, staged-root, and router inputs.
//!   Producer identity and the world timeline are controller-owned.
//!
//! The generated PROTO body lives in the same Webots project and the authored
//! world references it relatively.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use phoxal_model::Robot as RobotBundle;

use crate::simulation::webots::proto::{
    self, RobotInstance, WebotsController, externproto_for_generated_proto, generate_robot_proto,
    proto_name_for_robot, relative_mesh_url_prefix, render_robot_instance_node, stage_world,
};

/// The controller's controller artifact name (Webots `controller` field).
pub const WEBOTS_CONTROLLER_NAME: &str = "phoxal-simulator-webots-controller";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ControllerLaunch {
    pub robot_id: String,
    pub namespace: String,
    pub bundle_root: Option<PathBuf>,
    pub connect_endpoints: Vec<String>,
}

/// The staged PROTO subdirectory the generated robot PROTO's own
/// `EXTERNPROTO` declarations point at for its mounted components
/// (`webots_staging/render/proto.rs` hardcodes `"components/{proto_name}.proto"`
/// relative to the robot PROTO file - so component PROTOs must physically
/// live in a `components/` subdirectory of the robot's own PROTO directory).
const COMPONENT_PROTOS_SUBDIR: &str = "components";

/// One distinct simulated component type compiled into the canonical robot.
pub struct ComponentTypeToStage<'a> {
    pub component_type: &'a str,
}

/// One robot to stage into the simulation world: its bundle (manifest +
/// component specs + structure), the distinct component types it mounts, and
/// the stable launch inputs for the controller that substitutes its
/// component-driver contracts. `component_types` may be empty for a robot
/// with no mounted components (or none with compiled simulation semantics).
pub struct RobotToStage<'a> {
    pub robot_id: String,
    pub bundle: &'a RobotBundle,
    pub component_types: Vec<ComponentTypeToStage<'a>>,
    pub controller_launch: ControllerLaunch,
}

/// Everything the caller needs to launch the staged world.
pub struct StagedSimulationWorld {
    pub staged_world_path: PathBuf,
}

/// Stage a simulation world for exactly one robot: generate its PROTO (plus
/// one PROTO per distinct component type), render one static controller-bearing
/// instance node, and stage the augmented world text.
///
/// `staged_protos_dir` is where each generated robot `.proto` file is
/// written; component PROTOs go under its `components/` subdirectory (the
/// relative path the robot PROTO's own `EXTERNPROTO` declarations point at,
/// `proto::render::proto::component_externprotos`). `mesh_root` is
/// the staged mesh directory (`root::meshes_dir`) each
/// generated PROTO's mesh asset references are resolved relative to.
/// `staged_world_path` is where the staged `.wbt` text is written.
pub fn stage_simulation_world(
    base_world_text: &str,
    staged_protos_dir: &Path,
    mesh_root: &Path,
    staged_world_path: &Path,
    robots: &[RobotToStage<'_>],
) -> Result<StagedSimulationWorld> {
    std::fs::create_dir_all(staged_protos_dir).with_context(|| {
        format!(
            "failed to create staged protos directory {}",
            staged_protos_dir.display()
        )
    })?;
    // `relative_mesh_url_prefix` canonicalizes `mesh_root`, so it must exist
    // before any PROTO's mesh URL prefix is computed - a first `simulate` run
    // may not have created it yet.
    std::fs::create_dir_all(mesh_root).with_context(|| {
        format!(
            "failed to create staged meshes directory {}",
            mesh_root.display()
        )
    })?;
    if let Some(parent) = staged_world_path.parent() {
        std::fs::create_dir_all(parent).with_context(|| {
            format!(
                "failed to create staged world directory {}",
                parent.display()
            )
        })?;
    }

    let mut extern_protos = Vec::with_capacity(robots.len());
    anyhow::ensure!(
        robots.len() == 1,
        "a Webots simulation world must contain exactly one robot"
    );
    let mut static_robot_nodes = Vec::with_capacity(1);

    let component_protos_dir = staged_protos_dir.join(COMPONENT_PROTOS_SUBDIR);
    std::fs::create_dir_all(&component_protos_dir).with_context(|| {
        format!(
            "failed to create staged component protos directory {}",
            component_protos_dir.display()
        )
    })?;

    for robot in robots {
        let proto_name = proto_name_for_robot(robot.bundle.robot_id())?;
        let proto_path = staged_protos_dir.join(format!("{proto_name}.proto"));
        let mesh_url_prefix = relative_mesh_url_prefix(mesh_root, &proto_path)?;
        let component_solid_links = stage_component_protos(
            &component_protos_dir,
            mesh_root,
            robot.bundle,
            &robot.component_types,
        )
        .with_context(|| {
            format!(
                "failed to stage component PROTOs for robot '{}'",
                robot.robot_id
            )
        })?;

        let proto_text = generate_robot_proto(
            robot.bundle,
            robot.bundle.structure(),
            &component_solid_links,
            &proto_name,
            &mesh_url_prefix,
        )
        .with_context(|| format!("failed to generate PROTO for robot '{}'", robot.robot_id))?;
        std::fs::write(&proto_path, proto_text)
            .with_context(|| format!("failed to write staged PROTO {}", proto_path.display()))?;

        let extern_proto = externproto_for_generated_proto(&proto_path, staged_world_path)
            .with_context(|| {
                format!(
                    "failed to compute EXTERNPROTO reference for robot '{}'",
                    robot.robot_id
                )
            })?;
        extern_protos.push(extern_proto);

        let controller_args = controller_args(&robot.controller_launch)?;
        let instance = RobotInstance {
            proto_name: proto_name.clone(),
            def_name: proto_name.clone(),
            robot_id: robot.robot_id.clone(),
            controller: Some(WebotsController {
                controller_name: WEBOTS_CONTROLLER_NAME.to_string(),
                controller_args,
            }),
            supervisor: Some(false),
            synchronization: None,
        };
        let instance_node = render_robot_instance_node(
            robot.bundle,
            robot.bundle.structure(),
            &component_solid_links,
            &instance,
        )
        .with_context(|| {
            format!(
                "failed to render instance node for robot '{}'",
                robot.robot_id
            )
        })?;
        static_robot_nodes.push(instance_node);
    }

    let staged_text = stage_world(base_world_text, &extern_protos, &static_robot_nodes)
        .context("failed to stage simulation world")?;
    std::fs::write(staged_world_path, staged_text).with_context(|| {
        format!(
            "failed to write staged world {}",
            staged_world_path.display()
        )
    })?;

    Ok(StagedSimulationWorld {
        staged_world_path: staged_world_path.to_path_buf(),
    })
}

fn controller_args(launch: &ControllerLaunch) -> Result<Vec<String>> {
    let mut args = vec![
        "--robot-id".to_string(),
        launch.robot_id.clone(),
        "--namespace".to_string(),
        launch.namespace.clone(),
    ];
    if let Some(root) = &launch.bundle_root {
        args.extend(["--bundle-root".to_string(), root.display().to_string()]);
    }
    anyhow::ensure!(
        !launch.connect_endpoints.is_empty(),
        "Webots controller requires at least one router endpoint"
    );
    args.extend(["--connect".to_string(), launch.connect_endpoints.join(",")]);
    Ok(args)
}

/// Generate one PROTO per distinct component type the robot mounts, writing
/// each to `component_protos_dir` (the `components/` subdirectory the robot's
/// own generated PROTO's `EXTERNPROTO` declarations reference by relative
/// path). Returns the `component_solid_links` map
/// `generate_robot_proto`/`render_robot_instance_node` need, keyed by
/// component type (matching `WebotsSceneDescription::from_robot`'s
/// `component_solid_links.get(&model_component.component_type)` lookup).
fn stage_component_protos(
    component_protos_dir: &Path,
    mesh_root: &Path,
    bundle: &RobotBundle,
    component_types: &[ComponentTypeToStage<'_>],
) -> Result<BTreeMap<String, Vec<String>>> {
    let mut component_solid_links = BTreeMap::new();

    for component_type in component_types {
        let component_id = bundle
            .components()
            .find_map(|instance| {
                (instance.component_type() == component_type.component_type)
                    .then_some(instance.id())
            })
            .ok_or_else(|| {
                anyhow!(
                    "component type '{}' is not loaded in the robot bundle",
                    component_type.component_type
                )
            })?;
        let component_model = bundle.component_for_instance(component_id)?;
        let comp_simulation = bundle
            .simulation_for_component_type(component_type.component_type)
            .ok_or_else(|| {
                anyhow!(
                    "component type '{}' has no compiled simulation semantics",
                    component_type.component_type
                )
            })?;

        let comp_proto_name = proto_name_for_robot(component_type.component_type)?;
        let comp_proto_path = component_protos_dir.join(format!("{comp_proto_name}.proto"));
        let comp_mesh_url_prefix = relative_mesh_url_prefix(mesh_root, &comp_proto_path)?;

        let artifact = proto::generate_component_proto(
            component_type.component_type,
            component_model,
            component_model.structure(),
            comp_simulation,
            &comp_mesh_url_prefix,
        )
        .with_context(|| {
            format!(
                "failed to generate PROTO for component type '{}'",
                component_type.component_type
            )
        })?;
        std::fs::write(&comp_proto_path, artifact.proto_text).with_context(|| {
            format!(
                "failed to write staged component PROTO {}",
                comp_proto_path.display()
            )
        })?;
        component_solid_links.insert(
            component_type.component_type.to_string(),
            artifact.solid_link_ids,
        );
    }

    Ok(component_solid_links)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use phoxal_cli_core::identity::ExecutionId;

    const BASE_WORLD: &str = "#VRML_SIM R2025a utf8\nWorldInfo {}\n";

    fn fixture_bundle(component_source_dir: &Path) -> Result<RobotBundle> {
        let project_root = component_source_dir
            .parent()
            .and_then(Path::parent)
            .context("fixture component path has no project root")?;
        std::fs::create_dir_all(component_source_dir)?;
        std::fs::write(
            component_source_dir.join("structure.urdf"),
            r#"<robot name="drive">
  <link name="axle" />
  <link name="wheel" />
  <joint name="wheel_joint" type="continuous">
    <parent link="axle" />
    <child link="wheel" />
  </joint>
</robot>"#,
        )?;
        std::fs::write(
            project_root.join("robot.yaml"),
            r#"schema: robot/v0
robot:
  id: testbot
  namespace: dev
  motion_limits:
    max_linear_speed_mps: 0.6
    max_angular_speed_radps: 2.0
  kinematic:
    kind: omnidirectional
    actuators:
      - drive.motor
    encoders: []
  components:
    drive:
      component: drive
      mount_link: base_link
"#,
        )?;
        std::fs::write(
            component_source_dir.join("component.yaml"),
            r#"schema: component/v0
capabilities:
  motor:
    kind: motor
    command: velocity
    target:
      kind: joint
      id: wheel_joint
"#,
        )?;
        std::fs::write(
            component_source_dir.join("simulation.yaml"),
            "schema: simulation/v0\ncapabilities: {}\nlinks: {}\n",
        )?;
        std::fs::write(
            project_root.join("structure.urdf"),
            r#"<robot name="testbot">
  <link name="base_footprint" />
  <link name="base_link" />
  <joint name="root" type="fixed">
    <parent link="base_footprint" />
    <child link="base_link" />
  </joint>
</robot>"#,
        )?;
        let compiled = phoxal_manifest::compile(phoxal_manifest::SourceSet {
            project_root: project_root.to_path_buf(),
            robot_manifest: project_root.join("robot.yaml"),
            component_roots: BTreeMap::from([(
                "drive".to_string(),
                component_source_dir.to_path_buf(),
            )]),
        })?;
        Ok(compiled.into_parts().0)
    }

    #[test]
    fn stages_exactly_one_static_controller_robot_without_lifecycle_arguments() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let component_source_dir = temp.path().join("source-components/drive");
        let bundle = fixture_bundle(&component_source_dir)?;
        let launch = ControllerLaunch {
            namespace: "dev".to_string(),
            robot_id: "testbot".to_string(),
            bundle_root: Some(PathBuf::from("../../runtime")),
            connect_endpoints: vec![
                "unixsock-stream/a".to_string(),
                "tcp/localhost:7447".to_string(),
            ],
        };
        let world = temp.path().join("worlds/default.wbt");
        stage_simulation_world(
            BASE_WORLD,
            &temp.path().join("protos"),
            &temp.path().join("meshes"),
            &world,
            &[RobotToStage {
                robot_id: "testbot".to_string(),
                bundle: &bundle,
                component_types: vec![ComponentTypeToStage {
                    component_type: "drive",
                }],
                controller_launch: launch,
            }],
        )?;
        let text = std::fs::read_to_string(world)?;
        assert_eq!(
            text.matches(&format!("controller \"{WEBOTS_CONTROLLER_NAME}\""))
                .count(),
            1
        );
        assert!(!text.contains("supervisor TRUE"));
        assert!(!text.contains("--participant-id"));
        assert!(!text.contains("--producer"));
        assert!(!text.contains("--epoch"));
        assert!(text.contains("--bundle-root"));
        assert!(text.contains("../../runtime"));
        assert!(text.contains("unixsock-stream/a,tcp/localhost:7447"));
        assert!(temp.path().join("protos/components/Drive.proto").is_file());
        Ok(())
    }

    /// #952: the run identity reaches the controller through Webots'
    /// environment and nowhere else. The staged scene must therefore be a
    /// function of the robot model alone - two runs of the same project stage
    /// byte-identical worlds, and neither contains an execution id.
    ///
    /// This is what keeps the staged-content digest stable across runs, and
    /// what keeps the controller directory a run-invariant function of package
    /// content once controllers are installed packages (#951).
    #[test]
    fn the_staged_world_is_a_function_of_the_robot_model_not_of_the_run() -> Result<()> {
        let fixture = tempfile::tempdir()?;
        let component_source_dir = fixture.path().join("components/drive");
        let bundle = fixture_bundle(&component_source_dir)?;
        let launch = ControllerLaunch {
            namespace: "dev".to_string(),
            robot_id: "testbot".to_string(),
            bundle_root: Some(PathBuf::from("../../runtime")),
            connect_endpoints: vec!["tcp/localhost:7447".to_string()],
        };

        let mut staged = Vec::new();
        let executions = [ExecutionId::mint(), ExecutionId::mint()];
        for _ in &executions {
            let temp = tempfile::tempdir()?;
            let world = temp.path().join("worlds/default.wbt");
            stage_simulation_world(
                BASE_WORLD,
                &temp.path().join("protos"),
                &temp.path().join("meshes"),
                &world,
                &[RobotToStage {
                    robot_id: "testbot".to_string(),
                    bundle: &bundle,
                    component_types: vec![ComponentTypeToStage {
                        component_type: "drive",
                    }],
                    controller_launch: launch.clone(),
                }],
            )?;
            staged.push(std::fs::read_to_string(world)?);
        }

        assert_eq!(
            staged[0], staged[1],
            "two runs of one project must stage the same scene"
        );
        for execution in executions {
            assert!(
                !staged[0].contains(&execution.to_string()),
                "the run identity must not enter the staged world"
            );
        }
        Ok(())
    }

    #[derive(Debug, Parser)]
    struct ControllerCliShape {
        #[arg(long)]
        robot_id: String,
        #[arg(long)]
        namespace: String,
        #[arg(long)]
        bundle_root: Option<PathBuf>,
        #[arg(long)]
        connect: Option<String>,
    }

    #[test]
    fn controller_argv_round_trips_through_the_framework_clap_shape() -> Result<()> {
        let launch = ControllerLaunch {
            namespace: "dev".to_string(),
            robot_id: "testbot".to_string(),
            bundle_root: Some(PathBuf::from("../../runtime")),
            connect_endpoints: vec![
                "unixsock-stream/a".to_string(),
                "tcp/localhost:7447".to_string(),
            ],
        };
        let args = controller_args(&launch)?;
        let parsed = ControllerCliShape::try_parse_from(
            std::iter::once("controller").chain(args.iter().map(String::as_str)),
        )?;
        assert_eq!(parsed.robot_id, "testbot");
        assert_eq!(parsed.namespace, "dev");
        assert_eq!(parsed.bundle_root, Some(PathBuf::from("../../runtime")));
        assert_eq!(
            parsed.connect.as_deref(),
            Some("unixsock-stream/a,tcp/localhost:7447")
        );
        assert_eq!(args.iter().filter(|arg| *arg == "--connect").count(), 1);
        Ok(())
    }

    #[test]
    fn stages_a_mounted_component_proto_and_relative_world_reference() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let protos_dir = temp.path().join("protos");
        let mesh_root = temp.path().join("meshes");
        let world_path = temp.path().join("worlds/default.wbt");
        let component_source_dir = temp.path().join("source-components/drive");
        let bundle = fixture_bundle(&component_source_dir)?;
        let staged = stage_simulation_world(
            BASE_WORLD,
            &protos_dir,
            &mesh_root,
            &world_path,
            &[RobotToStage {
                robot_id: "testbot".to_string(),
                bundle: &bundle,
                component_types: vec![ComponentTypeToStage {
                    component_type: "drive",
                }],
                controller_launch: ControllerLaunch {
                    robot_id: "testbot".to_string(),
                    namespace: "dev".to_string(),
                    bundle_root: Some(PathBuf::from("../../runtime")),
                    connect_endpoints: vec!["unixsock-stream/router".to_string()],
                },
            }],
        )?;
        assert!(protos_dir.join("components/Drive.proto").is_file());
        let robot_proto = std::fs::read_to_string(protos_dir.join("Testbot.proto"))?;
        assert!(robot_proto.contains("components/Drive.proto"));
        let staged_world = std::fs::read_to_string(staged.staged_world_path)?;
        assert!(staged_world.contains("../protos/Testbot.proto"));
        Ok(())
    }
}
