use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{Result, anyhow};
use heck::ToUpperCamelCase;
use phoxal_engine::staged::Robot;
use phoxal_utils_simulation::v1::Simulation;
use phoxal_utils_structure::Structure;
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

use crate::unit::scene::WebotsSceneDescription;
use crate::unit::support::paths::{relative_path_for_asset, relative_path_for_world};
use crate::unit::world::{
    stage_world_source_with_proto, stage_world_source_with_text_fallback, validate_proto_document,
    validate_world_contact_materials,
};
use phoxal_cli_core::AppContext;
use phoxal_cli_core::unit::Unit;

#[derive(Debug, Clone)]
pub struct WebotsController {
    pub controller_name: String,
    pub controller_args: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct BuildWebotsScene<'a> {
    pub configuration: &'a Robot,
    pub structure: &'a Structure,
    pub component_solid_links: &'a BTreeMap<String, Vec<String>>,
}

impl Unit for BuildWebotsScene<'_> {
    type Output = WebotsSceneDescription;

    fn name(&self) -> &'static str {
        "Build Webots Scene"
    }

    fn execute(&self, _app: &AppContext) -> Result<Self::Output> {
        WebotsSceneDescription::from_robot(
            self.configuration,
            self.structure,
            self.component_solid_links,
        )
    }
}

#[derive(Debug, Clone)]
pub struct GenerateWebotsRobotProto<'a> {
    pub scene: &'a WebotsSceneDescription,
    pub proto_name: String,
    pub url_prefix: String,
}

impl Unit for GenerateWebotsRobotProto<'_> {
    type Output = Proto;

    fn name(&self) -> &'static str {
        "Generate Webots Robot Proto"
    }

    fn execute(&self, _app: &AppContext) -> Result<Self::Output> {
        self.scene
            .render_proto_document_with_mesh_url_prefix(&self.proto_name, &self.url_prefix)
    }
}

#[derive(Debug, Clone)]
pub struct GenerateWebotsRobotInstance<'a> {
    pub scene: &'a WebotsSceneDescription,
    pub proto_name: String,
    pub def_name: String,
    pub robot_id: String,
    pub controller: Option<WebotsController>,
    pub supervisor: Option<bool>,
    pub synchronization: Option<bool>,
}

impl Unit for GenerateWebotsRobotInstance<'_> {
    type Output = AstNode;

    fn name(&self) -> &'static str {
        "Generate Webots Robot Instance"
    }

    fn execute(&self, _app: &AppContext) -> Result<Self::Output> {
        self.scene.render_robot_instance_node(
            &self.proto_name,
            &self.def_name,
            &self.robot_id,
            self.controller.as_ref(),
            self.supervisor,
            self.synchronization,
        )
    }
}

#[derive(Debug, Clone)]
pub struct GenerateWebotsWorld<'a> {
    pub source_world: &'a str,
    pub extern_proto: ExternProto,
    pub root_nodes: Vec<AstNode>,
}

impl Unit for GenerateWebotsWorld<'_> {
    type Output = String;

    fn name(&self) -> &'static str {
        "Generate Webots World"
    }

    fn execute(&self, _app: &AppContext) -> Result<Self::Output> {
        match stage_world_source_with_proto(self.source_world, &self.extern_proto, &self.root_nodes)
        {
            Ok(staged) => Ok(staged),
            Err(parse_error) => stage_world_source_with_text_fallback(
                self.source_world,
                &self.extern_proto,
                &self.root_nodes,
            )
            .map_err(|fallback_error| {
                anyhow!(
                    "failed to stage world after webots-proto parse fallback: {parse_error:#}: {fallback_error:#}"
                )
            }),
        }
    }
}

#[derive(Debug, Clone)]
pub struct GenerateWebotsComponentProto<'a> {
    pub component_type: String,
    pub proto_name: String,
    pub structure: &'a Structure,
    pub component: &'a phoxal_utils_component::v1::Component,
    pub simulation: &'a Simulation,
    pub mesh_url_prefix: String,
}

impl Unit for GenerateWebotsComponentProto<'_> {
    type Output = (Proto, String, Vec<String>);

    fn name(&self) -> &'static str {
        "Generate Webots Component Proto"
    }

    fn execute(&self, _app: &AppContext) -> Result<Self::Output> {
        let scene = WebotsSceneDescription::from_component(
            &self.component_type,
            self.structure,
            self.component,
            self.simulation,
        )?;
        let comments = scene.runtime_metadata_comments();
        let proto =
            scene.render_component_proto_document(&self.proto_name, &self.mesh_url_prefix)?;
        Ok((proto, comments, scene.rendered_solid_link_ids()))
    }
}

#[derive(Debug, Clone)]
pub struct ValidateWebotsProto<'a> {
    pub proto_name: &'a str,
    pub proto: &'a Proto,
}

impl Unit for ValidateWebotsProto<'_> {
    type Output = ();

    fn name(&self) -> &'static str {
        "Validate Webots Proto"
    }

    fn execute(&self, _app: &AppContext) -> Result<Self::Output> {
        validate_proto_document(self.proto_name, self.proto)
    }
}

#[derive(Debug, Clone)]
pub struct ValidateWebotsWorld<'a> {
    pub staged_world: &'a str,
    pub referenced_contact_materials: &'a BTreeSet<String>,
}

impl Unit for ValidateWebotsWorld<'_> {
    type Output = ();

    fn name(&self) -> &'static str {
        "Validate Webots World"
    }

    fn execute(&self, _app: &AppContext) -> Result<Self::Output> {
        validate_world_contact_materials(self.staged_world, self.referenced_contact_materials)
    }
}

pub fn proto_name(robot_model: &str) -> Result<String> {
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

pub fn serialize_proto_document(
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
