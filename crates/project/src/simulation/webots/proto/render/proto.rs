use anyhow::{Result, anyhow};
use std::collections::BTreeSet;
use webots_proto::ast::proto::ast::{
    ArrayElement, ArrayValue, AstNode, AstNodeKind, ExternProto, FieldKeyword, FieldType, Header,
    NodeBodyElement, NodeField, ProtoBodyItem, ProtoDefinition, ProtoField as AstProtoField,
};
use webots_proto::ast::proto::span::Span;
use webots_proto::{Proto, r2025a_node_to_ast};

use crate::simulation::webots::proto::scene::WebotsSceneDescription;

impl WebotsSceneDescription {
    pub fn render_proto_document_with_mesh_url_prefix(
        &self,
        proto_name: &str,
        mesh_url_prefix: &str,
    ) -> Result<Proto> {
        let root_node = self.render_root_node_with_mesh_url_prefix(mesh_url_prefix)?;
        let root_ast = r2025a_node_to_ast(&root_node)
            .map_err(|error| anyhow!("failed to convert root node to AST: {error:?}"))?;
        let body_robot = self.render_proto_robot_ast(root_ast)?;

        Ok(Proto {
            header: Some(Header::new(
                "R2025a".to_string(),
                "utf8".to_string(),
                None,
                Span::default(),
            )),
            externprotos: self.component_externprotos(),
            proto: Some(
                ProtoDefinition::new(proto_name.to_string(), Span::default())
                    .with_fields(self.robot_proto_interface_fields())
                    .with_body(vec![ProtoBodyItem::Node(body_robot)]),
            ),
            root_nodes: Vec::new(),
            source_path: None,
            source_content: None,
        })
    }

    pub fn render_component_proto_document(
        &self,
        proto_name: &str,
        mesh_url_prefix: &str,
    ) -> Result<Proto> {
        let root_node = self.render_root_node_with_mesh_url_prefix(mesh_url_prefix)?;

        let body = match root_node {
            webots_proto::r2025a::Node::Solid(s) => webots_proto::r2025a::Node::Solid(
                s.with_translation(webots_proto::types::ProtoField::Is(
                    "translation".to_string(),
                ))
                .with_rotation(webots_proto::types::ProtoField::Is("rotation".to_string())),
            ),
            webots_proto::r2025a::Node::Pose(p) => webots_proto::r2025a::Node::Pose(
                p.with_translation(webots_proto::types::ProtoField::Is(
                    "translation".to_string(),
                ))
                .with_rotation(webots_proto::types::ProtoField::Is("rotation".to_string())),
            ),
            webots_proto::r2025a::Node::Transform(t) => webots_proto::r2025a::Node::Transform(
                t.with_translation(webots_proto::types::ProtoField::Is(
                    "translation".to_string(),
                ))
                .with_rotation(webots_proto::types::ProtoField::Is("rotation".to_string())),
            ),
            _ => webots_proto::r2025a::Node::Pose(
                webots_proto::r2025a::Pose::new()
                    .with_children(vec![root_node])
                    .with_translation(webots_proto::types::ProtoField::Is(
                        "translation".to_string(),
                    ))
                    .with_rotation(webots_proto::types::ProtoField::Is("rotation".to_string())),
            ),
        };

        let ast_node = r2025a_node_to_ast(&body)
            .map_err(|e| anyhow!("failed to convert component body to AST: {e:?}"))?;

        Ok(Proto {
            header: Some(Header::new(
                "R2025a".to_string(),
                "utf8".to_string(),
                None,
                Span::default(),
            )),
            externprotos: Vec::new(),
            proto: Some(
                ProtoDefinition::new(proto_name.to_string(), Span::default())
                    .with_fields(self.component_proto_interface_fields())
                    .with_body(vec![ProtoBodyItem::Node(ast_node)]),
            ),
            root_nodes: Vec::new(),
            source_path: None,
            source_content: None,
        })
    }

    fn component_externprotos(&self) -> Vec<ExternProto> {
        self.mounted_components_for_link
            .values()
            .flat_map(|instances| instances.iter())
            .map(|instance| instance.proto_name.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .map(|proto_name| {
                ExternProto::new(
                    format!("components/{proto_name}.proto"),
                    None,
                    Span::default(),
                )
            })
            .collect()
    }

    fn render_proto_robot_ast(&self, root_node: AstNode) -> Result<AstNode> {
        let mut fields = vec![
            NodeBodyElement::Field(NodeField::new(
                "translation".to_string(),
                webots_proto::FieldValue::Is("translation".to_string()),
                Span::default(),
            )),
            NodeBodyElement::Field(NodeField::new(
                "rotation".to_string(),
                webots_proto::FieldValue::Is("rotation".to_string()),
                Span::default(),
            )),
            NodeBodyElement::Field(NodeField::new(
                "name".to_string(),
                webots_proto::FieldValue::Is("name".to_string()),
                Span::default(),
            )),
            NodeBodyElement::Field(NodeField::new(
                "model".to_string(),
                webots_proto::FieldValue::String(self.robot_name.clone()),
                Span::default(),
            )),
            NodeBodyElement::Field(NodeField::new(
                "controller".to_string(),
                webots_proto::FieldValue::Is("controller".to_string()),
                Span::default(),
            )),
            NodeBodyElement::Field(NodeField::new(
                "controllerArgs".to_string(),
                webots_proto::FieldValue::Is("controllerArgs".to_string()),
                Span::default(),
            )),
            NodeBodyElement::Field(NodeField::new(
                "window".to_string(),
                webots_proto::FieldValue::Is("window".to_string()),
                Span::default(),
            )),
            NodeBodyElement::Field(NodeField::new(
                "customData".to_string(),
                webots_proto::FieldValue::Is("customData".to_string()),
                Span::default(),
            )),
            NodeBodyElement::Field(NodeField::new(
                "supervisor".to_string(),
                webots_proto::FieldValue::Is("supervisor".to_string()),
                Span::default(),
            )),
            NodeBodyElement::Field(NodeField::new(
                "synchronization".to_string(),
                webots_proto::FieldValue::Is("synchronization".to_string()),
                Span::default(),
            )),
        ];

        match root_node.kind {
            AstNodeKind::Node {
                type_name,
                def_name: _,
                fields: root_fields,
            } if type_name == "Solid" => {
                let mut children = None;
                let mut bounding_object = None;
                let mut physics = None;

                for field in root_fields {
                    let NodeBodyElement::Field(field) = field else {
                        continue;
                    };
                    match field.name.as_str() {
                        "children" => children = Some(field.value),
                        "boundingObject" => bounding_object = Some(field.value),
                        "physics" => physics = Some(field.value),
                        _ => {}
                    }
                }

                let mut children = match children {
                    Some(webots_proto::FieldValue::Array(children)) => children,
                    Some(other) => ArrayValue::new().with_elements(vec![ArrayElement::new(other)]),
                    None => ArrayValue::new(),
                };
                children.elements.extend(self.component_mount_nodes()?);
                fields.push(NodeBodyElement::Field(NodeField::new(
                    "children".to_string(),
                    webots_proto::FieldValue::Array(children),
                    Span::default(),
                )));

                if let Some(bounding_object) = bounding_object {
                    fields.push(NodeBodyElement::Field(NodeField::new(
                        "boundingObject".to_string(),
                        bounding_object,
                        Span::default(),
                    )));
                }
                if let Some(physics) = physics {
                    fields.push(NodeBodyElement::Field(NodeField::new(
                        "physics".to_string(),
                        physics,
                        Span::default(),
                    )));
                }
            }
            _ => {
                let mut children =
                    ArrayValue::new().with_elements(vec![ArrayElement::new(root_node.into())]);
                children.elements.extend(self.component_mount_nodes()?);
                fields.push(NodeBodyElement::Field(NodeField::new(
                    "children".to_string(),
                    webots_proto::FieldValue::Array(children),
                    Span::default(),
                )));
            }
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

    fn robot_proto_interface_fields(&self) -> Vec<AstProtoField> {
        vec![
            Self::ast_field(
                "translation",
                FieldType::SFVec3f,
                webots_proto::FieldValue::Vec3f([0.0, 0.0, 0.0]),
            ),
            Self::ast_field(
                "rotation",
                FieldType::SFRotation,
                webots_proto::FieldValue::Rotation([0.0, 0.0, 1.0, 0.0]),
            ),
            Self::ast_field(
                "name",
                FieldType::SFString,
                webots_proto::FieldValue::String(self.robot_name.clone()),
            ),
            Self::ast_field(
                "controller",
                FieldType::SFString,
                webots_proto::FieldValue::String("<extern>".to_string()),
            ),
            Self::ast_field(
                "controllerArgs",
                FieldType::MFString,
                webots_proto::FieldValue::Array(ArrayValue::new()),
            ),
            Self::ast_field(
                "window",
                FieldType::SFString,
                webots_proto::FieldValue::String("<generic>".to_string()),
            ),
            Self::ast_field(
                "customData",
                FieldType::SFString,
                webots_proto::FieldValue::String(String::new()),
            ),
            Self::ast_field(
                "supervisor",
                FieldType::SFBool,
                webots_proto::FieldValue::Bool(true),
            ),
            Self::ast_field(
                "synchronization",
                FieldType::SFBool,
                webots_proto::FieldValue::Bool(true),
            ),
        ]
    }

    fn component_proto_interface_fields(&self) -> Vec<AstProtoField> {
        let mut fields = vec![
            Self::ast_field(
                "translation",
                FieldType::SFVec3f,
                webots_proto::FieldValue::Vec3f([0.0, 0.0, 0.0]),
            ),
            Self::ast_field(
                "rotation",
                FieldType::SFRotation,
                webots_proto::FieldValue::Rotation([0.0, 0.0, 1.0, 0.0]),
            ),
        ];
        fields.extend(self.capability_name_interface_fields());
        fields.extend(self.solid_name_interface_fields());
        fields
    }

    fn capability_name_interface_fields(&self) -> Vec<AstProtoField> {
        self.capability_name_field_ids()
            .into_iter()
            .map(|capability_id| {
                Self::ast_hidden_field(
                    &Self::capability_name_field_name(&capability_id),
                    FieldType::SFString,
                    webots_proto::FieldValue::String(capability_id),
                )
            })
            .collect()
    }

    fn solid_name_interface_fields(&self) -> Vec<AstProtoField> {
        self.rendered_solid_link_ids()
            .into_iter()
            .map(|link_id| {
                Self::ast_hidden_field(
                    &Self::solid_name_field_name(&link_id),
                    FieldType::SFString,
                    webots_proto::FieldValue::String(format!("{}__{}", self.robot_name, link_id)),
                )
            })
            .collect()
    }

    fn ast_field(
        name: &str,
        field_type: FieldType,
        default_value: webots_proto::FieldValue,
    ) -> AstProtoField {
        AstProtoField::new(
            name.to_string(),
            field_type,
            FieldKeyword::Field,
            Span::default(),
        )
        .with_default_value(default_value)
    }

    fn ast_hidden_field(
        name: &str,
        field_type: FieldType,
        default_value: webots_proto::FieldValue,
    ) -> AstProtoField {
        AstProtoField::new(
            name.to_string(),
            field_type,
            FieldKeyword::HiddenField,
            Span::default(),
        )
        .with_default_value(default_value)
    }
}
