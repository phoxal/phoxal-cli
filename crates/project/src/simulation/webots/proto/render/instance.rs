use anyhow::{Result, anyhow};
use webots_proto::ast::proto::ast::{
    ArrayElement, ArrayValue, AstNode, AstNodeKind, NodeBodyElement, NodeField,
};
use webots_proto::ast::proto::span::Span;

use crate::simulation::webots::proto::WebotsController;
use crate::simulation::webots::proto::scene::{ComponentProtoInstance, WebotsSceneDescription};
use crate::simulation::webots::proto::support::pose::pose_to_isometry;

impl WebotsSceneDescription {
    pub fn render_robot_instance_node(
        &self,
        proto_name: &str,
        def_name: &str,
        robot_id: &str,
        controller: Option<&WebotsController>,
        supervisor: Option<bool>,
        synchronization: Option<bool>,
    ) -> Result<AstNode> {
        let root_link_id = self.rendered_root_link_id();
        let joint_from_parent = self
            .joints
            .values()
            .find(|joint| joint.child() == root_link_id);
        let mut fields = vec![
            NodeBodyElement::Field(NodeField::new(
                "translation".to_string(),
                {
                    let translation = Self::joint_translation(joint_from_parent);
                    webots_proto::FieldValue::Vec3f([translation.x, translation.y, translation.z])
                },
                Span::default(),
            )),
            NodeBodyElement::Field(NodeField::new(
                "rotation".to_string(),
                {
                    let rotation = Self::joint_rotation(joint_from_parent);
                    webots_proto::FieldValue::Rotation([
                        rotation.x,
                        rotation.y,
                        rotation.z,
                        rotation.angle,
                    ])
                },
                Span::default(),
            )),
        ];
        fields.push(NodeBodyElement::Field(NodeField::new(
            "name".to_string(),
            webots_proto::FieldValue::String(robot_id.to_string()),
            Span::default(),
        )));

        if let Some(controller) = controller {
            fields.push(NodeBodyElement::Field(NodeField::new(
                "controller".to_string(),
                webots_proto::FieldValue::String(controller.controller_name.clone()),
                Span::default(),
            )));
            if !controller.controller_args.is_empty() {
                fields.push(NodeBodyElement::Field(NodeField::new(
                    "controllerArgs".to_string(),
                    webots_proto::FieldValue::Array(
                        ArrayValue::new().with_elements(
                            controller
                                .controller_args
                                .iter()
                                .cloned()
                                .map(webots_proto::FieldValue::String)
                                .map(ArrayElement::new)
                                .collect(),
                        ),
                    ),
                    Span::default(),
                )));
            }
        }
        if let Some(supervisor) = supervisor {
            fields.push(NodeBodyElement::Field(NodeField::new(
                "supervisor".to_string(),
                webots_proto::FieldValue::Bool(supervisor),
                Span::default(),
            )));
        }
        if let Some(synchronization) = synchronization {
            fields.push(NodeBodyElement::Field(NodeField::new(
                "synchronization".to_string(),
                webots_proto::FieldValue::Bool(synchronization),
                Span::default(),
            )));
        }

        Ok(AstNode::new(
            AstNodeKind::Node {
                type_name: proto_name.to_string(),
                def_name: Some(def_name.to_string()),
                fields,
            },
            Span::default(),
        ))
    }

    pub fn component_mount_nodes(&self) -> Result<Vec<ArrayElement>> {
        let mut nodes = Vec::new();
        for (mount_link, instances) in &self.mounted_components_for_link {
            let transform = self.transform_from_rendered_root(mount_link)?;
            for instance in instances {
                nodes.push(ArrayElement::new(webots_proto::FieldValue::from(
                    self.render_component_instance_ast(instance, &transform),
                )));
            }
        }
        Ok(nodes)
    }

    fn render_component_instance_ast(
        &self,
        instance: &ComponentProtoInstance,
        transform: &nalgebra::Isometry3<f64>,
    ) -> AstNode {
        let rotation = Self::rotation_from_isometry(transform);
        let mut fields =
            Vec::with_capacity(instance.capability_names.len() + instance.solid_names.len() + 2);
        fields.push(NodeBodyElement::Field(NodeField::new(
            "translation".to_string(),
            webots_proto::FieldValue::Vec3f([
                transform.translation.vector.x,
                transform.translation.vector.y,
                transform.translation.vector.z,
            ]),
            Span::default(),
        )));
        fields.push(NodeBodyElement::Field(NodeField::new(
            "rotation".to_string(),
            webots_proto::FieldValue::Rotation([
                rotation.x,
                rotation.y,
                rotation.z,
                rotation.angle,
            ]),
            Span::default(),
        )));
        for (capability_id, capability_name) in &instance.capability_names {
            fields.push(NodeBodyElement::Field(NodeField::new(
                Self::capability_name_field_name(capability_id),
                webots_proto::FieldValue::String(capability_name.clone()),
                Span::default(),
            )));
        }
        for (link_id, solid_name) in &instance.solid_names {
            fields.push(NodeBodyElement::Field(NodeField::new(
                Self::solid_name_field_name(link_id),
                webots_proto::FieldValue::String(solid_name.clone()),
                Span::default(),
            )));
        }

        AstNode::new(
            AstNodeKind::Node {
                type_name: instance.proto_name.clone(),
                def_name: None,
                fields,
            },
            Span::default(),
        )
    }

    pub fn transform_from_rendered_root(&self, link_id: &str) -> Result<nalgebra::Isometry3<f64>> {
        let rendered_root_link_id = self.rendered_root_link_id();
        if link_id == rendered_root_link_id {
            return Ok(nalgebra::Isometry3::identity());
        }

        let joint = self
            .joints
            .values()
            .find(|joint| joint.child() == link_id)
            .ok_or_else(|| anyhow!("missing parent joint for link '{link_id}'"))?;

        let parent_transform = self.transform_from_rendered_root(joint.parent())?;
        Ok(parent_transform * pose_to_isometry(joint.origin()))
    }
}
