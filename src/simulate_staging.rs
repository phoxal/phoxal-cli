//! World-staging orchestration for `simulate` (Part 4).
//!
//! `src/webots_staging` is a real, tested render engine (robot PROTO
//! generation, instance-node rendering, world staging) with no caller before
//! this module. This module wires it into the `simulate` launch flow: given
//! the resolved robot + the base world + each robot's controller launch
//! contract, it stages a copy of the world that:
//!
//! - Declares an `EXTERNPROTO` for each robot's generated PROTO.
//! - Never adds a robot as a static instance node - robots are always
//!   supervisor-spawned at runtime, never pre-baked (see the module doc on
//!   `crate::simulation`).
//! - Adds exactly one static root node: the Webots supervisor `Robot` node
//!   (`supervisor TRUE`, `controller "phoxal-simulator-webots-supervisor"`).
//!   Its `--config` contains only non-spawn settings. Robot nodes are served
//!   separately over the bus by the live `simulate` launch path.
//!
//! Each spawn descriptor's `node_string` is a robot *instance* reference (PROTO
//! name + transform + `controller "phoxal-simulator-webots-controller"` +
//! `controllerArgs`), not the PROTO body - the body lives in the staged
//! `.proto` file the `EXTERNPROTO` declares.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use phoxal::model::simulation::Simulation as SimulationModel;
use phoxal::model::structure::Structure;
use phoxal::model::v0::Robot as RobotBundle;
use phoxal::participant::launch::ParticipantLaunch;
use phoxal_api::v0_1::simulation::RobotSpawn;
use serde::Serialize;

use crate::webots_staging::{
    self, RobotInstance, WebotsController, externproto_for_generated_proto, generate_robot_proto,
    proto_name_for_robot, relative_mesh_url_prefix, render_robot_instance_node, stage_world,
};
use phoxal_cli_core::session::launch_env::to_controller_args;

/// The supervisor's controller artifact name (Webots `controller` field).
pub const SUPERVISOR_CONTROLLER_NAME: &str = "phoxal-simulator-webots-supervisor";
/// The controller's controller artifact name (Webots `controller` field).
pub const CONTROLLER_CONTROLLER_NAME: &str = "phoxal-simulator-webots-controller";

/// The staged PROTO subdirectory the generated robot PROTO's own
/// `EXTERNPROTO` declarations point at for its mounted components
/// (`webots_staging/render/proto.rs` hardcodes `"components/{proto_name}.proto"`
/// relative to the robot PROTO file - so component PROTOs must physically
/// live in a `components/` subdirectory of the robot's own PROTO directory).
const COMPONENT_PROTOS_SUBDIR: &str = "components";

/// One distinct component type mounted on a robot: its on-disk source
/// directory (siblings `component.yaml` + `structure.urdf` + optionally
/// `simulation.yaml`, e.g. from `component_driver::component_crate_dir`), used
/// to generate that type's own PROTO once regardless of how many instances of
/// it the robot mounts.
pub struct ComponentTypeToStage<'a> {
    pub component_type: &'a str,
    pub source_dir: &'a Path,
}

/// One robot to stage into the simulation world: its bundle (manifest +
/// component specs + structure), the distinct component types it mounts, and
/// the `ParticipantLaunch` for the controller that substitutes its
/// component-driver contracts. `component_types` may be empty for a robot
/// with no mounted components (or none with a `simulation.yaml`).
pub struct RobotToStage<'a> {
    pub robot_id: String,
    pub bundle: &'a RobotBundle,
    pub component_types: Vec<ComponentTypeToStage<'a>>,
    pub controller_launch: ParticipantLaunch,
}

/// The supervisor `--config` payload. Spawn data is deliberately absent.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct SupervisorConfig {
    pub require_native: bool,
}

/// Everything the caller needs to launch the staged world and register its
/// simulation-managed participants (Part 5).
///
/// P4/C2 triage: `supervisor_launch`/`controller_launches` are real,
/// correctly-populated values (their EFFECT already lives in the staged
/// world's baked-in `controllerArgs`), but the real caller
/// (`crate::simulation::stage_and_prepare_webots_spec`) only needs
/// `staged_world_path`/`spawn_descriptors` afterward - these two exist for
/// test introspection of what was actually rendered
/// (`staged.supervisor_launch`/`staged.controller_launches` in this module's
/// and `crate::simulation`'s own tests), not because they are unused
/// byproducts.
pub struct StagedSimulationWorld {
    pub staged_world_path: PathBuf,
    #[allow(dead_code)]
    pub supervisor_launch: ParticipantLaunch,
    #[allow(dead_code)]
    pub controller_launches: Vec<(String, ParticipantLaunch)>,
    pub spawn_descriptors: Vec<RobotSpawn>,
}

/// Stage a simulation world for the given robots: generate each robot's
/// PROTO (plus one PROTO per distinct component type it mounts), render its
/// supervisor-owned instance node (never a static root node), build the
/// supervisor's static `Robot` node with non-spawn `--config`, and stage the
/// augmented world text. The returned descriptors are served over the bus by
/// the live launch path.
///
/// `staged_protos_dir` is where each generated robot `.proto` file is
/// written; component PROTOs go under its `components/` subdirectory (the
/// relative path the robot PROTO's own `EXTERNPROTO` declarations point at,
/// `webots_staging::render::proto::component_externprotos`). `mesh_root` is
/// the staged mesh directory (`webots_stage_root::meshes_dir`) each
/// generated PROTO's mesh asset references are resolved relative to.
/// `staged_world_path` is where the staged `.wbt` text is written.
pub fn stage_simulation_world(
    base_world_text: &str,
    staged_protos_dir: &Path,
    mesh_root: &Path,
    staged_world_path: &Path,
    supervisor_launch_base: ParticipantLaunch,
    require_native: bool,
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
    let mut spawn_descriptors = Vec::with_capacity(robots.len());
    let mut controller_launches = Vec::with_capacity(robots.len());

    let component_protos_dir = staged_protos_dir.join(COMPONENT_PROTOS_SUBDIR);
    std::fs::create_dir_all(&component_protos_dir).with_context(|| {
        format!(
            "failed to create staged component protos directory {}",
            component_protos_dir.display()
        )
    })?;

    for robot in robots {
        let proto_name = proto_name_for_robot(&robot.bundle.manifest.robot.id)?;
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
            &robot.bundle.structure,
            &component_solid_links,
            &proto_name,
            &mesh_url_prefix,
        )
        .with_context(|| format!("failed to generate PROTO for robot '{}'", robot.robot_id))?;
        std::fs::write(&proto_path, &proto_text)
            .with_context(|| format!("failed to write staged PROTO {}", proto_path.display()))?;

        let extern_proto = externproto_for_generated_proto(&proto_path, staged_world_path)
            .with_context(|| {
                format!(
                    "failed to compute EXTERNPROTO reference for robot '{}'",
                    robot.robot_id
                )
            })?;
        extern_protos.push(extern_proto);

        let controller_args = to_controller_args(&robot.controller_launch).with_context(|| {
            format!(
                "failed to render controller argv for robot '{}'",
                robot.robot_id
            )
        })?;
        let instance = RobotInstance {
            proto_name: proto_name.clone(),
            def_name: proto_name.clone(),
            robot_id: robot.robot_id.clone(),
            controller: Some(WebotsController {
                controller_name: CONTROLLER_CONTROLLER_NAME.to_string(),
                controller_args,
            }),
            supervisor: Some(false),
            synchronization: None,
        };
        let instance_node = render_robot_instance_node(
            robot.bundle,
            &robot.bundle.structure,
            &component_solid_links,
            &instance,
        )
        .with_context(|| {
            format!(
                "failed to render instance node for robot '{}'",
                robot.robot_id
            )
        })?;
        let node_string = render_node_to_string(&instance_node)?;

        spawn_descriptors.push(RobotSpawn {
            robot_id: robot.robot_id.clone(),
            node_string,
        });
        controller_launches.push((robot.robot_id.clone(), robot.controller_launch.clone()));
    }

    let supervisor_config = SupervisorConfig { require_native };
    let mut supervisor_launch = supervisor_launch_base;
    supervisor_launch.config = Some(
        serde_json::to_value(&supervisor_config)
            .context("failed to encode supervisor non-spawn config")?,
    );
    let supervisor_args =
        to_controller_args(&supervisor_launch).context("failed to render supervisor argv")?;

    let supervisor_node = supervisor_robot_node(supervisor_args)?;

    let staged_text = stage_world(base_world_text, &extern_protos, &[supervisor_node])
        .context("failed to stage simulation world")?;
    std::fs::write(staged_world_path, &staged_text).with_context(|| {
        format!(
            "failed to write staged world {}",
            staged_world_path.display()
        )
    })?;

    Ok(StagedSimulationWorld {
        staged_world_path: staged_world_path.to_path_buf(),
        supervisor_launch,
        controller_launches,
        spawn_descriptors,
    })
}

/// Generate one PROTO per distinct component type the robot mounts, writing
/// each to `component_protos_dir` (the `components/` subdirectory the robot's
/// own generated PROTO's `EXTERNPROTO` declarations reference by relative
/// path). Returns the `component_solid_links` map
/// `generate_robot_proto`/`render_robot_instance_node` need, keyed by
/// component type (matching `WebotsSceneDescription::from_robot`'s
/// `component_solid_links.get(&model_component.component)` lookup).
fn stage_component_protos(
    component_protos_dir: &Path,
    mesh_root: &Path,
    bundle: &RobotBundle,
    component_types: &[ComponentTypeToStage<'_>],
) -> Result<BTreeMap<String, Vec<String>>> {
    let mut component_solid_links = BTreeMap::new();

    for component_type in component_types {
        let component_model = bundle
            .components
            .get(component_type.component_type)
            .ok_or_else(|| {
                anyhow!(
                    "component type '{}' is not loaded in the robot bundle",
                    component_type.component_type
                )
            })?;

        let comp_structure =
            Structure::read_from_dir(component_type.source_dir).with_context(|| {
                format!(
                    "failed to read structure.urdf for component type '{}' from {}",
                    component_type.component_type,
                    component_type.source_dir.display()
                )
            })?;
        let comp_simulation = SimulationModel::read_from_dir(component_type.source_dir)
            .with_context(|| {
                format!(
                    "failed to read simulation.yaml for component type '{}' from {}",
                    component_type.component_type,
                    component_type.source_dir.display()
                )
            })?
            .as_v0()
            .context("Webots staging only supports simulation.yaml version v0")?
            .clone();

        let comp_proto_name = proto_name_for_robot(component_type.component_type)?;
        let comp_proto_path = component_protos_dir.join(format!("{comp_proto_name}.proto"));
        let comp_mesh_url_prefix = relative_mesh_url_prefix(mesh_root, &comp_proto_path)?;

        let artifact = webots_staging::generate_component_proto(
            component_type.component_type,
            component_model,
            &comp_structure,
            &comp_simulation,
            &comp_mesh_url_prefix,
        )
        .with_context(|| {
            format!(
                "failed to generate PROTO for component type '{}'",
                component_type.component_type
            )
        })?;
        std::fs::write(&comp_proto_path, &artifact.proto_text).with_context(|| {
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

/// Build the supervisor's static `Robot { supervisor TRUE controller "..."
/// controllerArgs [...] }` node. This IS added as a static root node of the
/// staged world - it is the only simulator-side static node; every robot is
/// supervisor-spawned instead (never both pre-baked as a static node AND
/// supervisor-spawned).
fn supervisor_robot_node(
    controller_args: Vec<String>,
) -> Result<webots_proto::ast::proto::ast::AstNode> {
    use webots_proto::ast::proto::ast::{AstNode, AstNodeKind, NodeBodyElement, NodeField};
    use webots_proto::ast::proto::span::Span;

    let mut fields = vec![
        NodeBodyElement::Field(NodeField::new(
            "controller".to_string(),
            webots_proto::FieldValue::String(SUPERVISOR_CONTROLLER_NAME.to_string()),
            Span::default(),
        )),
        NodeBodyElement::Field(NodeField::new(
            "supervisor".to_string(),
            webots_proto::FieldValue::Bool(true),
            Span::default(),
        )),
    ];
    if !controller_args.is_empty() {
        use webots_proto::ast::proto::ast::{ArrayElement, ArrayValue};
        fields.push(NodeBodyElement::Field(NodeField::new(
            "controllerArgs".to_string(),
            webots_proto::FieldValue::Array(
                ArrayValue::new().with_elements(
                    controller_args
                        .into_iter()
                        .map(webots_proto::FieldValue::String)
                        .map(ArrayElement::new)
                        .collect(),
                ),
            ),
            Span::default(),
        )));
    }

    Ok(AstNode::new(
        AstNodeKind::Node {
            type_name: "Robot".to_string(),
            def_name: None,
            fields,
        },
        Span::default(),
    ))
}

/// Serialize a single `AstNode` to canonical Webots text by wrapping it as
/// the sole root node of a headerless document - the same approach
/// `webots_staging::world::stage_world_source_with_text_fallback` uses to
/// render root nodes standalone, since `webots-proto`'s canonical writer only
/// exposes whole-document serialization.
fn render_node_to_string(node: &webots_proto::ast::proto::ast::AstNode) -> Result<String> {
    let document = webots_proto::Proto {
        header: None,
        externprotos: Vec::new(),
        proto: None,
        root_nodes: vec![node.clone()],
        source_path: None,
        source_content: None,
    };
    let rendered = document
        .to_canonical_string()
        .context("failed to serialize spawn descriptor node")?;
    Ok(rendered.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use phoxal::model::structure::Structure;
    use phoxal::participant::launch::{BusProfile, ClockMode, DEFAULT_SHUTDOWN_GRACE_MS};

    const BASE_WORLD: &str = "#VRML_SIM R2023b utf8\n\nWorldInfo {\n}\n";

    fn fixture_bundle(robot_id: &str) -> RobotBundle {
        let manifest_yaml = format!(
            r#"schema: robot/v0
robot:
  id: {robot_id}
  namespace: dev
  motion_limits:
    max_linear_speed_mps: 0.6
    max_angular_speed_radps: 2.0
  kinematic:
    kind: omnidirectional
    actuators: []
    encoders: []
  components: {{}}
"#
        );
        let manifest = phoxal::model::robot::v0::Robot::parse_from_string(&manifest_yaml)
            .expect("fixture manifest should parse");
        let structure = Structure::from_urdf_str(
            r#"<robot name="testbot">
  <link name="base_footprint" />
  <link name="base_link" />
  <joint name="root" type="fixed">
    <parent link="base_footprint" />
    <child link="base_link" />
    <origin xyz="0 0 0" rpy="0 0 0" />
  </joint>
</robot>
"#,
        )
        .expect("fixture structure should parse");
        RobotBundle {
            manifest,
            components: BTreeMap::new(),
            structure,
        }
    }

    fn fixture_launch(participant_id: &str, robot_id: &str) -> ParticipantLaunch {
        ParticipantLaunch {
            participant_id: participant_id.to_string(),
            incarnation: 0,
            namespace: "dev".to_string(),
            robot_id: robot_id.to_string(),
            bus: BusProfile {
                connect_endpoints: vec!["tcp/localhost:7447".to_string()],
            },
            clock: ClockMode::Simulation,
            config: None,
            robot_root: Some(PathBuf::from("/robot")),
            component_instance: None,
            shutdown_grace_ms: DEFAULT_SHUTDOWN_GRACE_MS,
        }
    }

    #[test]
    fn stages_world_with_supervisor_node_and_no_static_robot_instance() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let protos_dir = temp.path().join("protos");
        let mesh_root = temp.path().join("meshes");
        std::fs::create_dir_all(&mesh_root)?;
        let world_path = temp.path().join("worlds/default.wbt");

        let bundle = fixture_bundle("testbot");
        let mut controller_launch =
            fixture_launch("simulator-webots-controller-testbot", "testbot");
        controller_launch.config = Some(serde_json::json!({
            "message": "quoted \"value\" and backslash \\ with newline\nvisible",
        }));
        let supervisor_launch_base = fixture_launch("simulator-webots-supervisor", "testbot");

        let staged = stage_simulation_world(
            BASE_WORLD,
            &protos_dir,
            &mesh_root,
            &world_path,
            supervisor_launch_base,
            true,
            &[RobotToStage {
                robot_id: "testbot".to_string(),
                bundle: &bundle,
                component_types: Vec::new(),
                controller_launch: controller_launch.clone(),
            }],
        )?;

        let staged_text = std::fs::read_to_string(&staged.staged_world_path)?;

        // (a) an EXTERNPROTO for the robot's generated PROTO.
        assert!(
            staged_text.contains("EXTERNPROTO"),
            "staged world should declare an EXTERNPROTO:\n{staged_text}"
        );
        assert!(
            staged_text.contains("Testbot.proto") || staged_text.contains("Testbot\""),
            "staged world should reference the generated Testbot PROTO:\n{staged_text}"
        );

        // (b) the supervisor Robot node with `supervisor TRUE` + the
        // supervisor controller name.
        assert!(
            staged_text.contains("supervisor TRUE"),
            "staged world should contain the supervisor node:\n{staged_text}"
        );
        assert!(
            staged_text.contains(SUPERVISOR_CONTROLLER_NAME),
            "staged world should name the supervisor controller:\n{staged_text}"
        );

        // (c) no static robot instance node - only the supervisor `Robot`
        // node is a root node. The controller's controller name is carried
        // only in the separately returned bus spawn descriptor, never in the
        // staged world or supervisor `--config`.
        assert_eq!(
            staged_text
                .matches(&format!("controller \"{CONTROLLER_CONTROLLER_NAME}\""))
                .count(),
            0,
            "staged world root nodes must not carry an unescaped controller field for the robot controller:\n{staged_text}"
        );
        assert!(!staged_text.contains(CONTROLLER_CONTROLLER_NAME));
        assert_eq!(
            staged_text.matches("Robot {").count(),
            1,
            "the staged world should contain exactly one root Robot node (the supervisor):\n{staged_text}"
        );

        // The separately returned spawn descriptor carries the controller
        // controller name + a controllerArgs list for the bus responder.
        assert_eq!(staged.spawn_descriptors.len(), 1);
        let descriptor = &staged.spawn_descriptors[0];
        assert_eq!(descriptor.robot_id, "testbot");
        assert!(
            descriptor.node_string.contains(CONTROLLER_CONTROLLER_NAME),
            "{}",
            descriptor.node_string
        );
        assert!(
            descriptor.node_string.contains("controllerArgs"),
            "{}",
            descriptor.node_string
        );
        assert!(
            descriptor.node_string.contains("--participant-id"),
            "{}",
            descriptor.node_string
        );

        // The supervisor launch's config contains no spawn data.
        let config = staged
            .supervisor_launch
            .config
            .as_ref()
            .expect("supervisor launch should carry a config");
        assert_eq!(config["require_native"], serde_json::json!(true));
        assert!(config.get("spawn").is_none());
        assert!(config.get("node_string").is_none());
        let mut config_keys = config
            .as_object()
            .expect("supervisor config should be a JSON object")
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>();
        config_keys.sort_unstable();
        assert_eq!(config_keys, vec!["require_native"]);

        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct SupervisorConfigForTest {
            require_native: bool,
        }

        let config_argv = serde_json::to_string(config)?;
        let decoded: SupervisorConfigForTest = serde_json::from_str(&config_argv)?;
        assert!(decoded.require_native);
        assert!(descriptor.node_string.contains("--config"));
        assert!(descriptor.node_string.contains("quoted"));
        let parsed_descriptor: webots_proto::Proto = descriptor.node_string.parse()?;
        assert_eq!(parsed_descriptor.root_nodes.len(), 1);

        assert_eq!(staged.controller_launches.len(), 1);
        assert_eq!(staged.controller_launches[0].0, "testbot");

        Ok(())
    }

    #[test]
    fn stages_a_mounted_component_proto_alongside_the_robot_proto() -> Result<()> {
        use phoxal::model::component::v0::Component as ComponentSpec;

        let temp = tempfile::tempdir()?;
        let protos_dir = temp.path().join("protos");
        let mesh_root = temp.path().join("meshes");
        let world_path = temp.path().join("worlds/default.wbt");
        let component_source_dir = temp.path().join("components/ddsm115");
        std::fs::create_dir_all(&component_source_dir)?;
        std::fs::write(
            component_source_dir.join("structure.urdf"),
            r#"<robot name="ddsm115">
  <link name="wheel" />
</robot>
"#,
        )?;
        std::fs::write(
            component_source_dir.join("simulation.yaml"),
            "schema: simulation/v0\ncapabilities: {}\nlinks: {}\n",
        )?;

        let manifest_yaml = r#"schema: robot/v0
robot:
  id: testbot
  namespace: dev
  motion_limits:
    max_linear_speed_mps: 0.6
    max_angular_speed_radps: 2.0
  kinematic:
    kind: omnidirectional
    actuators: [left_drive.motor]
    encoders: []
  components:
    left_drive:
      component: ddsm115
      mount_link: base_link
      parameters:
        motor:
          kind: motor
"#;
        let manifest = phoxal::model::robot::v0::Robot::parse_from_string(manifest_yaml)?;
        let structure = Structure::from_urdf_str(
            r#"<robot name="testbot">
  <link name="base_footprint" />
  <link name="base_link" />
  <joint name="root" type="fixed">
    <parent link="base_footprint" />
    <child link="base_link" />
    <origin xyz="0 0 0" rpy="0 0 0" />
  </joint>
</robot>
"#,
        )?;
        let bundle = RobotBundle {
            manifest,
            components: BTreeMap::from([(
                "ddsm115".to_string(),
                ComponentSpec {
                    gtin: None,
                    capabilities: BTreeMap::new(),
                },
            )]),
            structure,
        };

        let controller_launch = fixture_launch("simulator-webots-controller-testbot", "testbot");
        let supervisor_launch_base = fixture_launch("simulator-webots-supervisor", "testbot");

        let staged = stage_simulation_world(
            BASE_WORLD,
            &protos_dir,
            &mesh_root,
            &world_path,
            supervisor_launch_base,
            false,
            &[RobotToStage {
                robot_id: "testbot".to_string(),
                bundle: &bundle,
                component_types: vec![ComponentTypeToStage {
                    component_type: "ddsm115",
                    source_dir: &component_source_dir,
                }],
                controller_launch,
            }],
        )?;

        let component_proto_path = protos_dir.join("components").join("Ddsm115.proto");
        assert!(
            component_proto_path.is_file(),
            "expected staged component PROTO at {}",
            component_proto_path.display()
        );
        let component_proto_text = std::fs::read_to_string(&component_proto_path)?;
        let parsed_component_proto: webots_proto::Proto = component_proto_text.parse()?;
        assert!(
            parsed_component_proto.proto.is_some(),
            "component PROTO should parse as a PROTO document:\n{component_proto_text}"
        );

        let robot_proto_path = protos_dir.join("Testbot.proto");
        assert!(
            robot_proto_path.is_file(),
            "expected staged robot PROTO at {}",
            robot_proto_path.display()
        );
        let robot_proto_text = std::fs::read_to_string(&robot_proto_path)?;
        assert!(
            robot_proto_text.contains("components/Ddsm115.proto"),
            "robot PROTO should reference mounted component PROTO relative to the robot PROTO:\n{robot_proto_text}"
        );
        let parsed_robot_proto: webots_proto::Proto = robot_proto_text.parse()?;
        assert!(
            parsed_robot_proto.proto.is_some(),
            "robot PROTO should parse as a PROTO document:\n{robot_proto_text}"
        );

        let staged_text = std::fs::read_to_string(&staged.staged_world_path)?;
        assert!(
            staged_text.contains("EXTERNPROTO"),
            "staged world should still declare the robot's own EXTERNPROTO:\n{staged_text}"
        );
        assert!(
            staged_text.contains("../protos/Testbot.proto"),
            "staged world should reference the robot PROTO relative to the .wbt path:\n{staged_text}"
        );

        Ok(())
    }
}
