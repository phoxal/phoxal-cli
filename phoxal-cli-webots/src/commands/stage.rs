use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use clap::Parser;
use phoxal_engine::RobotIdentity;
use phoxal_engine::staged::Robot;
use phoxal_utils_conventions::{COMPONENTS_DIR, DEFAULT_ROBOT_NAMESPACE, MESHES_DIR};
use phoxal_utils_helpers::parse_trimmed_non_empty;
use phoxal_utils_simulation::Simulation;
use phoxal_utils_structure::Structure;
use webots_proto::ast::proto::ast::{
    ArrayElement, ArrayValue, AstNode, AstNodeKind, NodeBodyElement, NodeField,
};
use webots_proto::ast::proto::span::Span;

use crate::unit::{
    BuildWebotsScene, GenerateWebotsComponentProto, GenerateWebotsRobotInstance,
    GenerateWebotsRobotProto, GenerateWebotsWorld, ValidateWebotsProto, ValidateWebotsWorld,
    WebotsController, externproto_for_generated_proto, proto_name, relative_mesh_url_prefix,
    serialize_proto_document,
};
use phoxal_cli_core::AppContext;
use phoxal_cli_core::unit::Unit;
use phoxal_cli_core::unit::robot::ValidatedRobot;
use phoxal_cli_core::utils::copy_dir_recursive;

use super::process::{build_host_binaries, host_binary_path};
use super::{
    CONTROLLER_BINARY, CONTROLLER_PACKAGE, INTERACTIVE_CONNECT_RETRIES,
    INTERACTIVE_CONNECT_TIMEOUT_MS, LOCAL_HOST_ROUTER_ENDPOINT, SUPERVISOR_BINARY,
    SUPERVISOR_PACKAGE,
};

#[derive(Debug, Parser, Clone)]
pub struct Stage {
    #[arg(help = "The robot model to stage.")]
    pub robot_model: String,

    #[arg(help = "The world to load.")]
    pub world: String,

    #[arg(
        long,
        help = "Optional robot ID override used to derive the robot hostname.",
        value_parser = parse_trimmed_non_empty
    )]
    pub robot_id: Option<String>,

    #[arg(
        long,
        help = "Robot namespace used to compose the robot hostname.",
        default_value_t = String::from(DEFAULT_ROBOT_NAMESPACE),
        value_parser = parse_trimmed_non_empty
    )]
    pub robot_namespace: String,
}
impl Stage {
    pub fn execute(&self, app: &AppContext) -> Result<()> {
        let identity = RobotIdentity::new(
            self.robot_id
                .clone()
                .unwrap_or_else(|| self.robot_model.clone()),
            self.robot_namespace.clone(),
        );
        let external_router_endpoint = LOCAL_HOST_ROUTER_ENDPOINT.to_string();
        let robot = app.ui.step("Validate Robot", || {
            ValidatedRobot::load(app, &self.robot_model)
        })?;
        let configuration = robot.stage_bundle(app, None)?;

        build_host_binaries(
            app,
            vec![
                SUPERVISOR_PACKAGE.to_string(),
                CONTROLLER_PACKAGE.to_string(),
            ],
        )?;

        let staged_world_path = stage_webots_project(StageWebotsProject {
            app,
            robot_model: &self.robot_model,
            world: &self.world,
            identity: &identity,
            external_router_endpoint: &external_router_endpoint,
            configuration: &configuration,
            model_structure: &robot.base_structure,
        })?;

        app.ui.info(format!(
            "Staged Webots artifacts for '{}' at {}",
            self.robot_model,
            staged_world_path.display()
        ));

        Ok(())
    }
}

pub struct StageWebotsProject<'a> {
    pub app: &'a AppContext,
    pub robot_model: &'a str,
    pub world: &'a str,
    pub identity: &'a RobotIdentity,
    pub external_router_endpoint: &'a str,
    pub configuration: &'a Robot,
    pub model_structure: &'a Structure,
}

pub fn stage_webots_project(input: StageWebotsProject<'_>) -> Result<PathBuf> {
    let StageWebotsProject {
        app,
        robot_model,
        world,
        identity,
        external_router_endpoint,
        configuration,
        model_structure,
    } = input;
    let world_source = if app.project.is_fixture_robot(robot_model) {
        app.project.fixture_world_source(world)
    } else {
        app.project.webots_world_source(world)
    };
    if !world_source.is_file() {
        bail!("world file not found: {}", world_source.display());
    }

    if app.project.staged_webots_root().exists() {
        fs::remove_dir_all(app.project.staged_webots_root())?;
    }
    fs::create_dir_all(app.project.staged_webots_protos_dir())?;
    fs::create_dir_all(app.project.staged_webots_protos_dir().join(COMPONENTS_DIR))?;
    fs::create_dir_all(app.project.staged_webots_meshes_dir())?;
    fs::create_dir_all(app.project.staged_webots_worlds_dir())?;
    fs::create_dir_all(app.project.staged_webots_controller_dir(SUPERVISOR_BINARY))?;
    fs::create_dir_all(app.project.staged_webots_controller_dir(CONTROLLER_BINARY))?;
    stage_webots_meshes(app, robot_model, configuration)?;
    let webots_mesh_root = app.project.staged_webots_meshes_dir();
    let component_solid_links =
        stage_component_protos(app, robot_model, configuration, &webots_mesh_root)?;

    let source_supervisor = host_binary_path(app.project.root(), SUPERVISOR_BINARY);
    let staged_supervisor = app.project.staged_webots_supervisor_binary();
    fs::copy(&source_supervisor, &staged_supervisor).with_context(|| {
        format!(
            "failed to copy Webots supervisor {} to {}",
            source_supervisor.display(),
            staged_supervisor.display()
        )
    })?;
    phoxal_cli_core::utils::make_executable(&staged_supervisor)?;

    let source_controller = host_binary_path(app.project.root(), CONTROLLER_BINARY);
    let staged_controller = app.project.staged_webots_controller_binary();
    fs::copy(&source_controller, &staged_controller).with_context(|| {
        format!(
            "failed to copy Webots controller {} to {}",
            source_controller.display(),
            staged_controller.display()
        )
    })?;
    phoxal_cli_core::utils::make_executable(&staged_controller)?;

    let scene = BuildWebotsScene {
        configuration,
        structure: model_structure,
        component_solid_links: &component_solid_links,
    }
    .run(app)?;
    let staged_world_path = app.project.staged_webots_world(world);
    let proto_name = proto_name(robot_model)?;
    let robot_def_name = robot_def_name(&identity.robot_id)?;
    validate_unique_robot_def_names(std::iter::once(identity.robot_id.as_str()))?;
    let generated_proto_path = app
        .project
        .staged_webots_root()
        .join("protos")
        .join(format!("{proto_name}.proto"));
    let mesh_url_prefix = relative_mesh_url_prefix(&webots_mesh_root, &generated_proto_path)?;
    let proto_document = GenerateWebotsRobotProto {
        scene: &scene,
        proto_name: proto_name.clone(),
        url_prefix: mesh_url_prefix,
    }
    .run(app)?;
    ValidateWebotsProto {
        proto_name: &proto_name,
        proto: &proto_document,
    }
    .run(app)?;
    let proto_source = serialize_proto_document(&proto_name, &proto_document, None)?;
    if let Some(parent) = generated_proto_path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&generated_proto_path, proto_source)
        .with_context(|| format!("failed to write {}", generated_proto_path.display()))?;

    let robot_node = GenerateWebotsRobotInstance {
        scene: &scene,
        proto_name: proto_name.clone(),
        def_name: robot_def_name,
        robot_id: identity.robot_id.clone(),
        controller: Some(WebotsController {
            controller_name: CONTROLLER_BINARY.to_string(),
            controller_args: build_controller_controller_args(identity, external_router_endpoint),
        }),
        supervisor: Some(false),
        synchronization: Some(true),
    }
    .run(app)?;
    let supervisor_node = build_supervisor_node(build_supervisor_controller_args(
        &identity.robot_namespace,
        external_router_endpoint,
    ));
    let externproto = externproto_for_generated_proto(&generated_proto_path, &staged_world_path)?;
    let source_world = fs::read_to_string(&world_source)
        .with_context(|| format!("failed to read {}", world_source.display()))?;
    let staged_world = GenerateWebotsWorld {
        source_world: &source_world,
        extern_proto: externproto,
        root_nodes: vec![supervisor_node, robot_node],
    }
    .run(app)?;
    let referenced_contact_materials =
        referenced_contact_materials(app, robot_model, configuration)?;
    ValidateWebotsWorld {
        staged_world: &staged_world,
        referenced_contact_materials: &referenced_contact_materials,
    }
    .run(app)?;
    if let Some(parent) = staged_world_path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&staged_world_path, staged_world)
        .with_context(|| format!("failed to write {}", staged_world_path.display()))?;
    stage_world_sidecar_directories(&world_source, app.project.staged_webots_worlds_dir())?;

    Ok(staged_world_path)
}

pub fn robot_def_name(robot_id: &str) -> Result<String> {
    let normalized = normalized_robot_id(robot_id);
    if normalized.is_empty() {
        bail!("robot id '{robot_id}' cannot be converted into a Webots DEF name");
    }
    Ok(format!("RF_ROBOT_{normalized}"))
}

fn normalized_robot_id(robot_id: &str) -> String {
    let mut normalized = String::new();
    let mut previous_was_separator = false;
    for character in robot_id.chars() {
        if character.is_ascii_alphanumeric() {
            normalized.push(character.to_ascii_uppercase());
            previous_was_separator = false;
        } else if !previous_was_separator {
            normalized.push('_');
            previous_was_separator = true;
        }
    }
    normalized.trim_matches('_').to_string()
}

pub fn validate_unique_robot_def_names<'a>(
    robot_ids: impl IntoIterator<Item = &'a str>,
) -> Result<()> {
    let mut seen = BTreeSet::new();
    for robot_id in robot_ids {
        let def_name = robot_def_name(robot_id)?;
        if !seen.insert(def_name.clone()) {
            bail!("robot id '{robot_id}' collides on staged Webots DEF '{def_name}'");
        }
    }
    Ok(())
}

fn stage_world_sidecar_directories(source_world: &Path, staged_worlds_dir: PathBuf) -> Result<()> {
    let Some(source_worlds_dir) = source_world.parent() else {
        return Ok(());
    };
    for entry in fs::read_dir(source_worlds_dir)? {
        let entry = entry?;
        let source_path = entry.path();
        if !source_path.is_dir() {
            continue;
        }
        let destination = staged_worlds_dir.join(entry.file_name());
        copy_dir_recursive(&source_path, &destination)?;
    }
    Ok(())
}

fn build_supervisor_controller_args(
    namespace: &str,
    external_router_endpoint: &str,
) -> Vec<String> {
    vec![
        "--robot-router-endpoint".to_string(),
        external_router_endpoint.to_string(),
        "--robot-connect-timeout-ms".to_string(),
        INTERACTIVE_CONNECT_TIMEOUT_MS.to_string(),
        "--robot-connect-retries".to_string(),
        INTERACTIVE_CONNECT_RETRIES.to_string(),
        "--robot-namespace".to_string(),
        namespace.to_string(),
    ]
}

fn build_controller_controller_args(
    identity: &RobotIdentity,
    external_router_endpoint: &str,
) -> Vec<String> {
    let mut args = vec![
        "--robot-id".to_string(),
        identity.robot_id.clone(),
        "--robot-router-endpoint".to_string(),
        external_router_endpoint.to_string(),
        "--robot-connect-timeout-ms".to_string(),
        INTERACTIVE_CONNECT_TIMEOUT_MS.to_string(),
        "--robot-connect-retries".to_string(),
        INTERACTIVE_CONNECT_RETRIES.to_string(),
    ];
    args.push("--robot-namespace".to_string());
    args.push(identity.robot_namespace.clone());
    args
}

fn build_supervisor_node(controller_args: Vec<String>) -> AstNode {
    let mut fields = vec![
        NodeBodyElement::Field(NodeField::new(
            "name".to_string(),
            webots_proto::FieldValue::String("rf_supervisor".to_string()),
            Span::default(),
        )),
        NodeBodyElement::Field(NodeField::new(
            "controller".to_string(),
            webots_proto::FieldValue::String(SUPERVISOR_BINARY.to_string()),
            Span::default(),
        )),
        NodeBodyElement::Field(NodeField::new(
            "supervisor".to_string(),
            webots_proto::FieldValue::Bool(true),
            Span::default(),
        )),
    ];
    if !controller_args.is_empty() {
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
    AstNode::new(
        AstNodeKind::Node {
            type_name: "Robot".to_string(),
            def_name: None,
            fields,
        },
        Span::default(),
    )
}

fn stage_component_protos(
    app: &AppContext,
    robot_model: &str,
    configuration: &Robot,
    mesh_root: &Path,
) -> Result<BTreeMap<String, Vec<String>>> {
    let mut generated_component_types = std::collections::HashSet::new();
    let mut component_solid_links = BTreeMap::<String, Vec<String>>::new();

    for component_type in configuration.model.used_component_types() {
        if generated_component_types.insert(component_type.to_string()) {
            let comp_config = configuration
                .components
                .get(component_type)
                .ok_or_else(|| anyhow!("component '{}' is not staged", component_type))?;

            let comp_sim = Simulation::read_from_dir(
                app.project
                    .component_simulation_dir_for_model(robot_model, component_type),
            )?;
            let comp_sim = comp_sim.as_v1().ok_or_else(|| {
                anyhow!(
                    "Webots simulation only supports simulation.yaml version v1 for {}",
                    component_type
                )
            })?;

            let comp_structure = Structure::read_from_dir(
                app.project
                    .component_structure_dir_for_model(robot_model, component_type),
            )?;

            let comp_proto_name = proto_name(component_type)?;
            let comp_proto_path = app
                .project
                .staged_webots_protos_dir()
                .join(COMPONENTS_DIR)
                .join(format!("{comp_proto_name}.proto"));

            let comp_mesh_url_prefix = relative_mesh_url_prefix(mesh_root, &comp_proto_path)?;

            let (comp_proto_doc, comp_comments, solid_link_ids) = GenerateWebotsComponentProto {
                component_type: component_type.to_string(),
                proto_name: comp_proto_name.clone(),
                structure: &comp_structure,
                component: comp_config,
                simulation: comp_sim,
                mesh_url_prefix: comp_mesh_url_prefix,
            }
            .run(app)?;
            component_solid_links.insert(component_type.to_string(), solid_link_ids);

            let comp_proto_source =
                serialize_proto_document(&comp_proto_name, &comp_proto_doc, Some(&comp_comments))?;
            fs::write(&comp_proto_path, comp_proto_source)
                .with_context(|| format!("failed to write {}", comp_proto_path.display()))?;
        }
    }

    Ok(component_solid_links)
}

fn stage_webots_meshes(app: &AppContext, robot_model: &str, configuration: &Robot) -> Result<()> {
    let mesh_root = app.project.staged_webots_meshes_dir();
    let model_mesh_dir = app.project.model_dir(robot_model).join(MESHES_DIR);
    if model_mesh_dir.is_dir() {
        copy_dir_recursive(&model_mesh_dir, &mesh_root)?;
    }

    for component_type in configuration.model.used_component_types() {
        let source = app.project.component_mesh_dir(robot_model, component_type);
        if source.is_dir() {
            copy_dir_recursive(&source, &mesh_root.join(component_type))?;
        }
    }

    Ok(())
}

fn referenced_contact_materials(
    app: &AppContext,
    robot_model: &str,
    configuration: &Robot,
) -> Result<std::collections::BTreeSet<String>> {
    let mut materials = std::collections::BTreeSet::new();

    for component_type in configuration.model.used_component_types() {
        let simulation = Simulation::read_from_dir(
            app.project
                .component_simulation_dir_for_model(robot_model, component_type),
        )?;
        let simulation = simulation.as_v1().ok_or_else(|| {
            anyhow!(
                "Webots simulation only supports simulation.yaml version v1 for {}",
                component_type
            )
        })?;

        materials.extend(
            simulation
                .links
                .values()
                .filter_map(|link| link.contact_material.clone()),
        );
    }

    Ok(materials)
}

#[cfg(test)]
mod tests {
    use super::{robot_def_name, validate_unique_robot_def_names};

    #[test]
    fn robot_def_name_normalizes_to_stable_webots_identifier() {
        assert_eq!(
            robot_def_name("robot-v1.alpha").unwrap(),
            "RF_ROBOT_ROBOT_V1_ALPHA"
        );
    }

    #[test]
    fn robot_def_name_rejects_ids_without_ascii_identifier_content() {
        assert!(robot_def_name("---").is_err());
    }

    #[test]
    fn robot_def_name_collision_is_rejected() {
        let result = validate_unique_robot_def_names(["robot-v1", "robot_v1"]);

        assert!(result.is_err());
    }
}
