use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use phoxal::model::simulation::Simulation;
use phoxal::model::structure::Structure;
use phoxal::model::v1::Robot;
use webots_proto::FieldValue;
use webots_proto::ast::proto::ast::{
    ArrayElement, ArrayValue, AstNode, AstNodeKind, NodeBodyElement, NodeField,
};
use webots_proto::ast::proto::span::Span;

use crate::host_paths;
use crate::resolver::{ResolvedComponentSource, ResolvedRobot};
use crate::utils::{copy_dir_recursive, make_executable};
use crate::webots_staging::{RobotInstance, WebotsController};

const SIMULATOR_WEBOTS_CONTROLLER: &str = "simulator_webots_controller";
const SIMULATOR_WEBOTS_SUPERVISOR: &str = "simulator_webots_supervisor";

pub fn stage_webots_artifacts(
    project_root: &Path,
    resolved: &ResolvedRobot,
    run_dir: &Path,
    world_path: &Path,
    router_endpoint: &str,
) -> Result<()> {
    let webots_dir = project_root.join(".phoxal").join("webots");
    if webots_dir.exists() {
        fs::remove_dir_all(&webots_dir)
            .with_context(|| format!("failed to clear {}", webots_dir.display()))?;
    }
    fs::create_dir_all(&webots_dir)
        .with_context(|| format!("failed to create {}", webots_dir.display()))?;

    let source_world_path = world_path.to_path_buf();
    let source_world = fs::read_to_string(&source_world_path).with_context(|| {
        format!(
            "world source missing at {} - copy a world template (e.g. phoxal/framework/fixture/world/ArenaWorld.wbt) to that path",
            world_path.display()
        )
    })?;

    let robot = Robot::read_from_dir(run_dir)
        .with_context(|| format!("failed to read staged robot from {}", run_dir.display()))?;
    let structure = Structure::read_from_dir(run_dir)
        .with_context(|| format!("failed to read staged structure from {}", run_dir.display()))?;

    let proto_dir = webots_dir.join("protos");
    let component_proto_dir = proto_dir.join("components");
    let mesh_dir = webots_dir.join("meshes");
    let worlds_dir = webots_dir.join("worlds");
    let controllers_dir = webots_dir.join("controllers");
    fs::create_dir_all(&component_proto_dir)
        .with_context(|| format!("failed to create {}", component_proto_dir.display()))?;
    fs::create_dir_all(&mesh_dir)
        .with_context(|| format!("failed to create {}", mesh_dir.display()))?;
    fs::create_dir_all(&worlds_dir)
        .with_context(|| format!("failed to create {}", worlds_dir.display()))?;
    fs::create_dir_all(&controllers_dir)
        .with_context(|| format!("failed to create {}", controllers_dir.display()))?;

    stage_meshes(project_root, resolved, &mesh_dir)?;

    let mut component_solid_links = BTreeMap::<String, Vec<String>>::new();
    let mut referenced_contact_materials = BTreeSet::<String>::new();
    for component_type in robot.manifest.used_component_types() {
        let component = robot
            .components
            .get(component_type)
            .ok_or_else(|| anyhow!("component '{}' is not staged", component_type))?;
        let component_dir = run_dir.join("components").join(component_type);
        let simulation = read_simulation_v1(&component_dir, component_type)?;
        referenced_contact_materials.extend(
            simulation
                .links
                .values()
                .filter_map(|link| link.contact_material.clone()),
        );
        let component_structure = Structure::read_from_dir(&component_dir).with_context(|| {
            format!(
                "failed to read staged structure for component '{}' from {}",
                component_type,
                component_dir.display()
            )
        })?;
        let proto_name = crate::webots_staging::proto_name_for_robot(component_type)?;
        let component_proto_path = component_proto_dir.join(format!("{proto_name}.proto"));
        let mesh_url_prefix =
            crate::webots_staging::relative_mesh_url_prefix(&mesh_dir, &component_proto_path)?;
        let artifact = crate::webots_staging::generate_component_proto(
            component_type,
            component,
            &component_structure,
            &simulation,
            &mesh_url_prefix,
        )
        .with_context(|| format!("failed to generate Webots PROTO for {component_type}"))?;
        fs::write(&component_proto_path, artifact.proto_text).with_context(|| {
            format!(
                "failed to write component PROTO {}",
                component_proto_path.display()
            )
        })?;
        component_solid_links.insert(component_type.to_string(), artifact.solid_link_ids);
    }

    let proto_name = crate::webots_staging::proto_name_for_robot(&resolved.robot.identity.id)?;
    let robot_proto_path = proto_dir.join(format!("{proto_name}.proto"));
    let mesh_url_prefix =
        crate::webots_staging::relative_mesh_url_prefix(&mesh_dir, &robot_proto_path)?;
    let robot_proto = crate::webots_staging::generate_robot_proto(
        &robot,
        &structure,
        &component_solid_links,
        &proto_name,
        &mesh_url_prefix,
    )?;
    fs::write(&robot_proto_path, robot_proto)
        .with_context(|| format!("failed to write robot PROTO {}", robot_proto_path.display()))?;

    let instance = RobotInstance {
        proto_name,
        def_name: robot_def_name(&resolved.robot.identity.id)?,
        robot_id: resolved.robot.identity.id.clone(),
        controller: Some(WebotsController {
            controller_name: "phoxal-simulator-webots-controller".to_string(),
            controller_args: controller_args(resolved, router_endpoint),
        }),
        supervisor: Some(false),
        synchronization: Some(true),
    };
    let robot_node = crate::webots_staging::render_robot_instance_node(
        &robot,
        &structure,
        &component_solid_links,
        &instance,
    )?;
    let supervisor_node = build_supervisor_node(controller_args(resolved, router_endpoint));
    let world_path = worlds_dir.join("default.wbt");
    let externproto =
        crate::webots_staging::externproto_for_generated_proto(&robot_proto_path, &world_path)?;
    let staged_world = crate::webots_staging::stage_world(
        &source_world,
        &[externproto],
        &[supervisor_node, robot_node],
    )?;
    crate::webots_staging::validate_world_contact_materials(
        &staged_world,
        &referenced_contact_materials,
    )?;
    fs::write(&world_path, staged_world)
        .with_context(|| format!("failed to write staged world {}", world_path.display()))?;
    stage_world_sidecar_directories(&source_world_path, &worlds_dir)?;
    stage_controller_binaries(resolved, &controllers_dir)?;

    Ok(())
}

fn read_simulation_v1(
    component_dir: &Path,
    component_type: &str,
) -> Result<phoxal::model::simulation::v1::Simulation> {
    let simulation = Simulation::read_from_dir(component_dir).with_context(|| {
        format!(
            "failed to read simulation.yaml for component '{}' from {}",
            component_type,
            component_dir.display()
        )
    })?;
    let Simulation::V1(simulation) = simulation;
    Ok(simulation)
}

fn stage_meshes(project_root: &Path, resolved: &ResolvedRobot, mesh_dir: &Path) -> Result<()> {
    copy_dir_if_present_and_empty(&project_root.join("meshes"), mesh_dir)?;
    let host_cache_dir = host_paths::cache_dir()?;
    for component in &resolved.components {
        let source_dir = component_source_dir(project_root, &host_cache_dir, component);
        let source_meshes = source_dir.join("meshes");
        let destination = mesh_dir.join(component.source_name.as_str());
        copy_dir_if_present_and_empty(&source_meshes, &destination)?;
    }
    Ok(())
}

fn component_source_dir(
    project_root: &Path,
    host_cache_dir: &Path,
    component: &crate::resolver::ResolvedComponent,
) -> PathBuf {
    match &component.source {
        ResolvedComponentSource::Path { path } => {
            if path.is_absolute() {
                path.clone()
            } else {
                project_root.join(path)
            }
        }
        ResolvedComponentSource::Git { commit, .. } => host_cache_dir
            .join("components")
            .join(format!("{}-{commit}", component.source_name)),
    }
}

fn copy_dir_if_present_and_empty(source: &Path, destination: &Path) -> Result<()> {
    if !source.is_dir() || directory_has_entries(destination)? {
        return Ok(());
    }
    copy_dir_recursive(source, destination)
}

fn directory_has_entries(path: &Path) -> Result<bool> {
    if !path.is_dir() {
        return Ok(false);
    }
    Ok(fs::read_dir(path)
        .with_context(|| format!("failed to read {}", path.display()))?
        .next()
        .transpose()?
        .is_some())
}

fn build_supervisor_node(controller_args: Vec<String>) -> AstNode {
    AstNode::new(
        AstNodeKind::Node {
            type_name: "Robot".to_string(),
            def_name: None,
            fields: vec![
                NodeBodyElement::Field(NodeField::new(
                    "name".to_string(),
                    FieldValue::String("rf_supervisor".to_string()),
                    Span::default(),
                )),
                NodeBodyElement::Field(NodeField::new(
                    "controller".to_string(),
                    FieldValue::String("phoxal-simulator-webots-supervisor".to_string()),
                    Span::default(),
                )),
                NodeBodyElement::Field(NodeField::new(
                    "controllerArgs".to_string(),
                    FieldValue::Array(
                        ArrayValue::new().with_elements(
                            controller_args
                                .into_iter()
                                .map(FieldValue::String)
                                .map(ArrayElement::new)
                                .collect(),
                        ),
                    ),
                    Span::default(),
                )),
                NodeBodyElement::Field(NodeField::new(
                    "supervisor".to_string(),
                    FieldValue::Bool(true),
                    Span::default(),
                )),
            ],
        },
        Span::default(),
    )
}

fn controller_args(resolved: &ResolvedRobot, router_endpoint: &str) -> Vec<String> {
    vec![
        "--robot-id".to_string(),
        resolved.robot.identity.id.clone(),
        "--robot-namespace".to_string(),
        resolved.robot.identity.namespace.clone(),
        "--robot-router-endpoint".to_string(),
        router_endpoint.to_string(),
    ]
}

fn stage_controller_binaries(resolved: &ResolvedRobot, controllers_dir: &Path) -> Result<()> {
    for tool_name in [SIMULATOR_WEBOTS_CONTROLLER, SIMULATOR_WEBOTS_SUPERVISOR] {
        let Some(tool) = resolved.tools.iter().find(|tool| tool.name == tool_name) else {
            tracing::warn!(
                "resolved simulator tool {tool_name} is missing; Webots will fail to spawn controllers"
            );
            continue;
        };
        let source = cached_tool_path(&tool.name, &tool.resolved, &tool.binary_name)?;
        if !source.is_file() {
            tracing::warn!(
                "cached simulator binary not found at {}; Webots will fail to spawn controllers (a live `simulate` provisions it; a dry-run does not)",
                source.display()
            );
            continue;
        }
        let controller_name = webots_controller_name(&tool.binary_name)?;
        let destination_dir = controllers_dir.join(&controller_name);
        fs::create_dir_all(&destination_dir)
            .with_context(|| format!("failed to create {}", destination_dir.display()))?;
        let destination = destination_dir.join(&controller_name);
        fs::copy(&source, &destination).with_context(|| {
            format!(
                "failed to copy simulator controller {} to {}",
                source.display(),
                destination.display()
            )
        })?;
        make_executable(&destination)?;
    }
    Ok(())
}

pub(crate) fn cached_tool_path(name: &str, version: &str, binary_name: &str) -> Result<PathBuf> {
    Ok(host_paths::tools_cache_dir()?
        .join(name)
        .join(version)
        .join(binary_name))
}

fn webots_controller_name(binary_name: &str) -> Result<String> {
    let target = crate::resolver::host_target_triple();
    binary_name
        .strip_suffix(&format!("-{target}"))
        .map(ToString::to_string)
        .ok_or_else(|| {
            anyhow!(
                "simulator tool binary '{binary_name}' does not end with host target suffix '-{target}'"
            )
        })
}

fn robot_def_name(robot_id: &str) -> Result<String> {
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

fn stage_world_sidecar_directories(source_world: &Path, worlds_dir: &Path) -> Result<()> {
    let Some(source_worlds_dir) = source_world.parent() else {
        return Ok(());
    };
    for entry in fs::read_dir(source_worlds_dir)
        .with_context(|| format!("failed to read {}", source_worlds_dir.display()))?
    {
        let entry = entry?;
        let source_path = entry.path();
        if !source_path.is_dir() {
            continue;
        }
        copy_dir_recursive(&source_path, &worlds_dir.join(entry.file_name()))?;
    }
    Ok(())
}
