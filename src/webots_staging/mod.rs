use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{Result, anyhow};
use heck::ToUpperCamelCase;
use phoxal::model::simulation::v1::Simulation;
use phoxal::model::structure::Structure;
use phoxal::model::v1::Robot;
use webots_proto::Proto;
use webots_proto::ast::proto::ast::{AstNode, ExternProto};
use webots_proto::ast::proto::span::Span;

mod metadata;
mod native_fields;
mod render;
pub mod scene;
mod support;
mod types;
mod world;

use crate::webots_staging::scene::WebotsSceneDescription;
use crate::webots_staging::support::paths::{relative_path_for_asset, relative_path_for_world};
use crate::webots_staging::world::{
    stage_world_source_with_protos, stage_world_source_with_text_fallback, validate_proto_document,
};

#[derive(Debug, Clone)]
pub struct ComponentProtoArtifact {
    pub proto_text: String,
    pub solid_link_ids: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct RobotInstance {
    pub proto_name: String,
    pub def_name: String,
    pub robot_id: String,
    pub controller: Option<WebotsController>,
    pub supervisor: Option<bool>,
    pub synchronization: Option<bool>,
}

#[derive(Debug, Clone)]
pub struct WebotsController {
    pub controller_name: String,
    pub controller_args: Vec<String>,
}

pub fn generate_component_proto(
    component_type: &str,
    component: &phoxal::model::component::v1::Component,
    structure: &Structure,
    simulation: &Simulation,
    mesh_url_prefix: &str,
) -> Result<ComponentProtoArtifact> {
    let proto_name = proto_name_for_robot(component_type)?;
    let scene =
        WebotsSceneDescription::from_component(component_type, structure, component, simulation)?;
    let comments = scene.runtime_metadata_comments();
    let proto = scene.render_component_proto_document(&proto_name, mesh_url_prefix)?;
    let solid_link_ids = scene.rendered_solid_link_ids();
    let proto_text = serialize_proto_document(&proto_name, &proto, Some(&comments))?;
    Ok(ComponentProtoArtifact {
        proto_text,
        solid_link_ids,
    })
}

pub fn generate_robot_proto(
    robot: &Robot,
    structure: &Structure,
    component_solid_links: &BTreeMap<String, Vec<String>>,
    proto_name: &str,
    mesh_url_prefix: &str,
) -> Result<String> {
    let scene = build_webots_scene(robot, structure, component_solid_links)?;
    let proto = scene.render_proto_document_with_mesh_url_prefix(proto_name, mesh_url_prefix)?;
    serialize_proto_document(proto_name, &proto, None)
}

pub fn render_robot_instance_node(
    robot: &Robot,
    structure: &Structure,
    component_solid_links: &BTreeMap<String, Vec<String>>,
    instance: &RobotInstance,
) -> Result<AstNode> {
    let scene = build_webots_scene(robot, structure, component_solid_links)?;
    scene.render_robot_instance_node(
        &instance.proto_name,
        &instance.def_name,
        &instance.robot_id,
        instance.controller.as_ref(),
        instance.supervisor,
        instance.synchronization,
    )
}

pub fn stage_world(
    source_world: &str,
    extern_protos: &[ExternProto],
    root_nodes: &[AstNode],
) -> Result<String> {
    match stage_world_source_with_protos(source_world, extern_protos, root_nodes) {
        Ok(staged) => Ok(staged),
        Err(parse_error) => {
            stage_world_source_with_text_fallback(source_world, extern_protos, root_nodes).map_err(
                |fallback_error| {
                    anyhow!(
                        "failed to stage world after webots-proto parse fallback: {parse_error:#}: {fallback_error:#}"
                    )
                },
            )
        }
    }
}

pub fn validate_world_contact_materials(
    staged_world: &str,
    referenced_contact_materials: &BTreeSet<String>,
) -> Result<()> {
    world::validate_world_contact_materials(staged_world, referenced_contact_materials)
}

pub fn proto_name_for_robot(robot_model: &str) -> Result<String> {
    let proto_name = robot_model.to_upper_camel_case();
    if proto_name.is_empty() {
        return Err(anyhow!(
            "robot model '{}' cannot be converted into a Webots PROTO name",
            robot_model
        ));
    }
    Ok(proto_name)
}

pub fn relative_mesh_url_prefix(mesh_root: &Path, generated_proto_path: &Path) -> Result<String> {
    relative_path_for_asset(mesh_root, generated_proto_path)
}

pub fn externproto_for_generated_proto(
    generated_proto_path: &Path,
    world_path: &Path,
) -> Result<ExternProto> {
    let externproto_path = relative_path_for_world(generated_proto_path, world_path)?;
    Ok(ExternProto::new(externproto_path, None, Span::default()))
}

fn build_webots_scene(
    configuration: &Robot,
    structure: &Structure,
    component_solid_links: &BTreeMap<String, Vec<String>>,
) -> Result<WebotsSceneDescription> {
    WebotsSceneDescription::from_robot(configuration, structure, component_solid_links)
}

fn serialize_proto_document(
    proto_name: &str,
    proto: &Proto,
    comments: Option<&str>,
) -> Result<String> {
    validate_proto_document(proto_name, proto)?;
    let mut s = proto.to_canonical_string().map_err(|error| {
        anyhow!("failed to serialize generated Webots PROTO '{proto_name}': {error:#}")
    })?;

    if let Some(c) = comments
        && !c.is_empty()
        && let Some(pos) = s.find('\n')
    {
        s.insert_str(pos + 1, c);
    }

    Ok(s)
}
