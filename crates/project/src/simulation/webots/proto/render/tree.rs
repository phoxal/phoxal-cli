use std::collections::BTreeSet;

use anyhow::{Result, anyhow};
use phoxal_model::structure::{Joint, JointKind};
use webots_proto::r2025a::{
    HingeJoint, HingeJointParameters, JointParameters, Node, Physics, Pose, SliderJoint, Solid,
};
use webots_proto::types::{ProtoField as WebotsField, SFRotation, SFVec3f};

use crate::simulation::webots::proto::native_fields::{
    NativeValue, native_webots_contact_material_for_link,
};
use crate::simulation::webots::proto::scene::WebotsSceneDescription;
use crate::simulation::webots::proto::support::inertia::transform_inertial;
use crate::simulation::webots::proto::support::pose::{
    JointType, convert_joint_type, has_inertial, pose_to_isometry,
};
use crate::simulation::webots::proto::types::{FixedSubtreeRender, StagedCollision};

impl WebotsSceneDescription {
    pub fn render_root_node_with_mesh_url_prefix(&self, mesh_url_prefix: &str) -> Result<Node> {
        self.render_link_subtree_with_mesh_url_prefix(
            self.rendered_root_link_id().as_str(),
            mesh_url_prefix,
        )
    }

    pub fn rendered_root_link_id(&self) -> String {
        let mut link_id = self.root_link_id.clone();
        loop {
            let Some(link) = self.links.get(&link_id) else {
                return link_id;
            };
            if has_inertial(link)
                || link.visuals().len() != 0
                || link.collisions().len() != 0
                || self
                    .mounted_components_for_link
                    .contains_key(link_id.as_str())
            {
                return link_id;
            }

            let next_child_link_id = {
                let mut child_joints = self.child_joints(link_id.as_str());
                match child_joints.next() {
                    Some(child_joint)
                        if child_joints.next().is_none()
                            && child_joint.kind() == JointKind::Fixed =>
                    {
                        Some(child_joint.child().to_string())
                    }
                    _ => None,
                }
            };
            let Some(next_child_link_id) = next_child_link_id else {
                return link_id;
            };
            if next_child_link_id == link_id {
                return link_id;
            }
            link_id = next_child_link_id;
        }
    }

    pub fn render_link_subtree_with_mesh_url_prefix(
        &self,
        link_id: &str,
        mesh_url_prefix: &str,
    ) -> Result<Node> {
        let joint_from_parent = self.joints.values().find(|joint| joint.child() == link_id);

        if !self.is_physical_link(link_id) {
            let mut children = Vec::new();
            for joint in self.child_joints(link_id) {
                children.push(self.render_joint(joint, mesh_url_prefix)?);
            }
            return Ok(Node::Pose(
                Pose::new()
                    .with_translation(Self::joint_translation(joint_from_parent))
                    .with_rotation(Self::joint_rotation(joint_from_parent))
                    .with_children(children),
            ));
        }

        let assembly =
            self.collect_fixed_subtree(link_id, &nalgebra::Isometry3::identity(), mesh_url_prefix)?;
        let mut solid = Solid::new(link_id.to_string())
            .with_translation(Self::joint_translation(joint_from_parent))
            .with_rotation(Self::joint_rotation(joint_from_parent))
            .with_children(assembly.children);
        if self.is_component_proto {
            solid.name = WebotsField::Is(Self::solid_name_field_name(link_id));
        }

        if let Some(assignment) = native_webots_contact_material_for_link(
            self.contact_materials.get(link_id).map(|s| s.as_str()),
        ) && let NativeValue::String(material_name) = assignment.value
        {
            solid = solid.with_contact_material(material_name);
        }

        if let Some(bounding_object) =
            self.render_assembly_bounding_object(&assembly.collisions, mesh_url_prefix)?
        {
            solid = solid.with_bounding_object(Box::new(bounding_object));
        }
        if let Some(inertial) = assembly.mass_properties.finalize() {
            solid = solid.with_physics(Box::new(Node::Physics(
                Physics::new()
                    .with_density(-1.0)
                    .with_mass(inertial.mass)
                    .with_center_of_mass(Self::vec3([
                        inertial.origin.translation.vector.x,
                        inertial.origin.translation.vector.y,
                        inertial.origin.translation.vector.z,
                    ]))
                    .with_inertia_matrix(vec![
                        inertial.inertia.ixx,
                        inertial.inertia.iyy,
                        inertial.inertia.izz,
                        inertial.inertia.ixy,
                        inertial.inertia.ixz,
                        inertial.inertia.iyz,
                    ]),
            )));
        }

        Ok(Node::Solid(solid))
    }

    fn render_joint(&self, joint: &Joint, mesh_url_prefix: &str) -> Result<Node> {
        let child_body =
            self.render_link_subtree_with_mesh_url_prefix(joint.child(), mesh_url_prefix)?;
        let axis = Self::vec3(joint.axis());
        let joint_type = convert_joint_type(joint.kind())?;

        match joint_type {
            JointType::Fixed => Ok(child_body),
            JointType::Continuous | JointType::Revolute => {
                let damping = joint.dynamics().map(|value| value.damping()).unwrap_or(0.0);
                let friction = joint
                    .dynamics()
                    .map(|value| value.friction())
                    .unwrap_or(0.0);
                let joint_origin = pose_to_isometry(joint.origin());
                let anchor = Self::vec3([
                    joint_origin.translation.vector.x,
                    joint_origin.translation.vector.y,
                    joint_origin.translation.vector.z,
                ]);

                let mut joint_parameters = HingeJointParameters::new()
                    .with_axis(axis)
                    .with_anchor(anchor)
                    .with_damping_constant(damping)
                    .with_static_friction(friction);
                if joint_type == JointType::Revolute {
                    let limit = joint.limit();
                    joint_parameters = joint_parameters
                        .with_min_stop(limit.lower())
                        .with_max_stop(limit.upper());
                }

                let mut hinge_joint = HingeJoint::new()
                    .with_joint_parameters(Box::new(Node::HingeJointParameters(joint_parameters)))
                    .with_end_point(Box::new(child_body));
                let capabilities = self.render_joint_capabilities(joint.name());
                if !capabilities.is_empty() {
                    hinge_joint = hinge_joint.with_device(capabilities);
                }
                Ok(Node::HingeJoint(hinge_joint))
            }
            JointType::Prismatic => {
                let damping = joint.dynamics().map(|value| value.damping()).unwrap_or(0.0);
                let friction = joint
                    .dynamics()
                    .map(|value| value.friction())
                    .unwrap_or(0.0);
                let limit = joint.limit();
                let mut slider_joint = SliderJoint::new()
                    .with_joint_parameters(Box::new(Node::JointParameters(
                        JointParameters::new()
                            .with_axis(axis)
                            .with_min_stop(limit.lower())
                            .with_max_stop(limit.upper())
                            .with_damping_constant(damping)
                            .with_static_friction(friction),
                    )))
                    .with_end_point(Box::new(child_body));
                let capabilities = self.render_joint_capabilities(joint.name());
                if !capabilities.is_empty() {
                    slider_joint = slider_joint.with_device(capabilities);
                }
                Ok(Node::SliderJoint(slider_joint))
            }
        }
    }

    fn collect_fixed_subtree(
        &self,
        link_id: &str,
        transform_to_root: &nalgebra::Isometry3<f64>,
        mesh_url_prefix: &str,
    ) -> Result<FixedSubtreeRender> {
        let link = self
            .links
            .get(link_id)
            .ok_or_else(|| anyhow!("missing link '{}'", link_id))?;
        let mut assembly = FixedSubtreeRender::default();

        for visual in link.visuals() {
            let origin = transform_to_root * pose_to_isometry(visual.origin());
            assembly
                .children
                .push(self.render_visual(visual.geometry(), &origin, mesh_url_prefix));
        }
        for binding in self.link_bindings(link_id) {
            if let Some(node) = self.render_link_capability(
                binding.capability_id.as_str(),
                &binding.physical,
                binding.simulation.as_ref(),
            ) {
                assembly
                    .children
                    .push(Self::wrap_with_pose(transform_to_root, node));
            }
        }
        for collision in link.collisions() {
            assembly.collisions.push(StagedCollision {
                origin: transform_to_root * pose_to_isometry(collision.origin()),
                geometry: collision.geometry().clone(),
            });
        }
        if has_inertial(link) {
            assembly
                .mass_properties
                .add_inertial(&transform_inertial(link.inertial(), transform_to_root));
        }
        for joint in self.child_joints(link_id) {
            if joint.kind() == JointKind::Fixed {
                let child_transform = transform_to_root * pose_to_isometry(joint.origin());
                assembly.extend(self.collect_fixed_subtree(
                    joint.child(),
                    &child_transform,
                    mesh_url_prefix,
                )?);
            } else {
                assembly.children.push(Self::wrap_with_pose(
                    transform_to_root,
                    self.render_joint(joint, mesh_url_prefix)?,
                ));
            }
        }

        Ok(assembly)
    }

    fn is_physical_link(&self, link_id: &str) -> bool {
        let Some(link) = self.links.get(link_id) else {
            return false;
        };
        link.visuals().len() != 0
            || link.collisions().len() != 0
            || has_inertial(link)
            || !self.link_bindings(link_id).is_empty()
            || self.mounted_components_for_link.contains_key(link_id)
    }

    pub fn rendered_solid_link_ids(&self) -> Vec<String> {
        let mut link_ids = BTreeSet::new();
        self.collect_rendered_solid_link_ids_from(
            self.rendered_root_link_id().as_str(),
            &mut link_ids,
        );
        link_ids.into_iter().collect()
    }

    fn collect_rendered_solid_link_ids_from(&self, link_id: &str, link_ids: &mut BTreeSet<String>) {
        if !self.is_physical_link(link_id) {
            for joint in self.child_joints(link_id) {
                self.collect_rendered_solid_link_ids_from(joint.child(), link_ids);
            }
            return;
        }

        link_ids.insert(link_id.to_string());
        self.collect_jointed_rendered_solid_link_ids_under_fixed_subtree(link_id, link_ids);
    }

    fn collect_jointed_rendered_solid_link_ids_under_fixed_subtree(
        &self,
        link_id: &str,
        link_ids: &mut BTreeSet<String>,
    ) {
        for joint in self.child_joints(link_id) {
            if joint.kind() == JointKind::Fixed {
                self.collect_jointed_rendered_solid_link_ids_under_fixed_subtree(
                    joint.child(),
                    link_ids,
                );
            } else {
                self.collect_rendered_solid_link_ids_from(joint.child(), link_ids);
            }
        }
    }

    fn wrap_with_pose(transform: &nalgebra::Isometry3<f64>, node: Node) -> Node {
        if Self::is_identity_transform(transform) {
            node
        } else {
            Node::Pose(
                Pose::new()
                    .with_translation(Self::vec3([
                        transform.translation.vector.x,
                        transform.translation.vector.y,
                        transform.translation.vector.z,
                    ]))
                    .with_rotation(Self::rotation_from_isometry(transform))
                    .with_children(vec![node]),
            )
        }
    }

    fn is_identity_transform(transform: &nalgebra::Isometry3<f64>) -> bool {
        transform.translation.vector.norm() <= 1.0e-12 && transform.rotation.angle() <= 1.0e-12
    }

    pub fn joint_translation(joint: Option<&Joint>) -> SFVec3f {
        joint
            .map(|joint| {
                let joint_origin = pose_to_isometry(joint.origin());
                Self::vec3([
                    joint_origin.translation.vector.x,
                    joint_origin.translation.vector.y,
                    joint_origin.translation.vector.z,
                ])
            })
            .unwrap_or_else(|| Self::vec3([0.0, 0.0, 0.0]))
    }

    pub fn joint_rotation(joint: Option<&Joint>) -> SFRotation {
        joint
            .map(|joint| Self::rotation_from_isometry(&pose_to_isometry(joint.origin())))
            .unwrap_or_else(|| Self::rotation([0.0, 0.0, 1.0, 0.0]))
    }

    pub fn rotation_from_isometry(transform: &nalgebra::Isometry3<f64>) -> SFRotation {
        if let Some((axis, angle)) = transform.rotation.axis_angle() {
            Self::rotation([axis.x, axis.y, axis.z, angle])
        } else {
            Self::rotation([0.0, 0.0, 1.0, 0.0])
        }
    }

    pub fn lookup_table_from_native(values: &[[f64; 3]]) -> Vec<SFVec3f> {
        values
            .iter()
            .map(|&[v0, v1, v2]| Self::vec3([v0, v1, v2]))
            .collect()
    }

    pub fn vec3(values: [f64; 3]) -> SFVec3f {
        SFVec3f::new(values[0], values[1], values[2])
    }

    pub fn rotation(values: [f64; 4]) -> SFRotation {
        SFRotation::new(values[0], values[1], values[2], values[3])
    }
}
