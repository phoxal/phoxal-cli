use std::collections::BTreeSet;

use anyhow::{Result, anyhow};
use webots_proto::r2025a::{
    HingeJoint, HingeJointParameters, JointParameters, Node, Physics, Pose, SliderJoint, Solid,
};
use webots_proto::types::{ProtoField as WebotsField, SFRotation, SFVec3f};

use crate::unit::native_fields::{NativeValue, native_webots_contact_material_for_link};
use crate::unit::scene::WebotsSceneDescription;
use crate::unit::support::inertia::transform_inertial;
use crate::unit::support::urdf::{JointType, convert_joint_type, has_inertial, pose_to_isometry};
use crate::unit::types::{FixedSubtreeRender, StagedCollision};

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
                || !link.visual.is_empty()
                || !link.collision.is_empty()
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
                            && child_joint.joint_type == urdf_rs::JointType::Fixed =>
                    {
                        Some(child_joint.child.link.clone())
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
        let joint_from_parent = self
            .joints
            .values()
            .find(|joint| joint.child.link == link_id);

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
        if self.component_mesh_prefix.is_some() {
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

    fn render_joint(&self, joint: &urdf_rs::Joint, mesh_url_prefix: &str) -> Result<Node> {
        let child_body =
            self.render_link_subtree_with_mesh_url_prefix(&joint.child.link, mesh_url_prefix)?;
        let axis = Self::vec3([joint.axis.xyz[0], joint.axis.xyz[1], joint.axis.xyz[2]]);
        let joint_type = convert_joint_type(&joint.joint_type)?;

        match joint_type {
            JointType::Fixed => Ok(child_body),
            JointType::Continuous | JointType::Revolute => {
                let damping = joint.dynamics.as_ref().map(|d| d.damping).unwrap_or(0.0);
                let friction = joint.dynamics.as_ref().map(|d| d.friction).unwrap_or(0.0);
                let joint_origin = pose_to_isometry(&joint.origin);
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
                    joint_parameters = joint_parameters
                        .with_min_stop(joint.limit.lower)
                        .with_max_stop(joint.limit.upper);
                }

                let mut hinge_joint = HingeJoint::new()
                    .with_joint_parameters(Box::new(Node::HingeJointParameters(joint_parameters)))
                    .with_end_point(Box::new(child_body));
                let capabilities = self.render_joint_capabilities(joint.name.as_str());
                if !capabilities.is_empty() {
                    hinge_joint = hinge_joint.with_device(capabilities);
                }
                Ok(Node::HingeJoint(hinge_joint))
            }
            JointType::Prismatic => {
                let damping = joint.dynamics.as_ref().map(|d| d.damping).unwrap_or(0.0);
                let friction = joint.dynamics.as_ref().map(|d| d.friction).unwrap_or(0.0);
                let mut slider_joint = SliderJoint::new()
                    .with_joint_parameters(Box::new(Node::JointParameters(
                        JointParameters::new()
                            .with_axis(axis)
                            .with_min_stop(joint.limit.lower)
                            .with_max_stop(joint.limit.upper)
                            .with_damping_constant(damping)
                            .with_static_friction(friction),
                    )))
                    .with_end_point(Box::new(child_body));
                let capabilities = self.render_joint_capabilities(joint.name.as_str());
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

        for visual in &link.visual {
            let origin = transform_to_root * pose_to_isometry(&visual.origin);
            assembly
                .children
                .push(self.render_visual(&visual.geometry, &origin, mesh_url_prefix));
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
        for collision in &link.collision {
            assembly.collisions.push(StagedCollision {
                origin: transform_to_root * pose_to_isometry(&collision.origin),
                geometry: collision.geometry.clone(),
            });
        }
        if has_inertial(link) {
            assembly
                .mass_properties
                .add_inertial(&transform_inertial(&link.inertial, transform_to_root));
        }
        for joint in self.child_joints(link_id) {
            if joint.joint_type == urdf_rs::JointType::Fixed {
                let child_transform = transform_to_root * pose_to_isometry(&joint.origin);
                assembly.extend(self.collect_fixed_subtree(
                    &joint.child.link,
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
        !link.visual.is_empty()
            || !link.collision.is_empty()
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
                self.collect_rendered_solid_link_ids_from(joint.child.link.as_str(), link_ids);
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
            if joint.joint_type == urdf_rs::JointType::Fixed {
                self.collect_jointed_rendered_solid_link_ids_under_fixed_subtree(
                    joint.child.link.as_str(),
                    link_ids,
                );
            } else {
                self.collect_rendered_solid_link_ids_from(joint.child.link.as_str(), link_ids);
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

    pub fn joint_translation(joint: Option<&urdf_rs::Joint>) -> SFVec3f {
        joint
            .map(|joint| {
                let joint_origin = pose_to_isometry(&joint.origin);
                Self::vec3([
                    joint_origin.translation.vector.x,
                    joint_origin.translation.vector.y,
                    joint_origin.translation.vector.z,
                ])
            })
            .unwrap_or_else(|| Self::vec3([0.0, 0.0, 0.0]))
    }

    pub fn joint_rotation(joint: Option<&urdf_rs::Joint>) -> SFRotation {
        joint
            .map(|joint| Self::rotation_from_isometry(&pose_to_isometry(&joint.origin)))
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

#[cfg(test)]
mod tests {
    use super::WebotsSceneDescription;
    use std::collections::BTreeMap;
    use webots_proto::r2025a::Node;

    fn pose(x: f64, y: f64, z: f64) -> urdf_rs::Pose {
        urdf_rs::Pose {
            xyz: urdf_rs::Vec3([x, y, z]),
            rpy: urdf_rs::Vec3([0.0, 0.0, 0.0]),
        }
    }

    fn inertia(ixx: f64, iyy: f64, izz: f64) -> urdf_rs::Inertia {
        urdf_rs::Inertia {
            ixx,
            ixy: 0.0,
            ixz: 0.0,
            iyy,
            iyz: 0.0,
            izz,
        }
    }

    fn inertial(origin: urdf_rs::Pose, mass: f64, inertia: urdf_rs::Inertia) -> urdf_rs::Inertial {
        urdf_rs::Inertial {
            origin,
            mass: urdf_rs::Mass { value: mass },
            inertia,
        }
    }

    fn box_collision(size: [f64; 3]) -> urdf_rs::Collision {
        urdf_rs::Collision {
            name: None,
            origin: pose(0.0, 0.0, 0.0),
            geometry: urdf_rs::Geometry::Box {
                size: urdf_rs::Vec3(size),
            },
        }
    }

    fn cylinder_collision(radius: f64, length: f64) -> urdf_rs::Collision {
        urdf_rs::Collision {
            name: None,
            origin: pose(0.0, 0.0, 0.0),
            geometry: urdf_rs::Geometry::Cylinder { radius, length },
        }
    }

    fn link(
        name: &str,
        collisions: Vec<urdf_rs::Collision>,
        inertial: Option<urdf_rs::Inertial>,
    ) -> urdf_rs::Link {
        urdf_rs::Link {
            name: name.to_string(),
            inertial: inertial.unwrap_or_default(),
            visual: Vec::new(),
            collision: collisions,
        }
    }

    fn fixed_joint(name: &str, parent: &str, child: &str, origin: urdf_rs::Pose) -> urdf_rs::Joint {
        urdf_rs::Joint {
            name: name.to_string(),
            joint_type: urdf_rs::JointType::Fixed,
            origin,
            parent: urdf_rs::LinkName {
                link: parent.to_string(),
            },
            child: urdf_rs::LinkName {
                link: child.to_string(),
            },
            axis: urdf_rs::Axis::default(),
            limit: urdf_rs::JointLimit::default(),
            calibration: None,
            dynamics: None,
            mimic: None,
            safety_controller: None,
        }
    }

    fn revolute_joint(
        name: &str,
        parent: &str,
        child: &str,
        origin: urdf_rs::Pose,
        axis: [f64; 3],
        lower: f64,
        upper: f64,
    ) -> urdf_rs::Joint {
        urdf_rs::Joint {
            name: name.to_string(),
            joint_type: urdf_rs::JointType::Revolute,
            origin,
            parent: urdf_rs::LinkName {
                link: parent.to_string(),
            },
            child: urdf_rs::LinkName {
                link: child.to_string(),
            },
            axis: urdf_rs::Axis {
                xyz: urdf_rs::Vec3(axis),
            },
            limit: urdf_rs::JointLimit {
                lower,
                upper,
                effort: 0.0,
                velocity: 0.0,
            },
            calibration: None,
            dynamics: None,
            mimic: None,
            safety_controller: None,
        }
    }

    fn render_test_root(scene: &WebotsSceneDescription) -> anyhow::Result<Node> {
        scene.render_root_node_with_mesh_url_prefix("")
    }

    #[test]
    fn fixed_physical_descendant_through_mount_chain_is_folded_into_parent_solid()
    -> anyhow::Result<()> {
        let scene = WebotsSceneDescription {
            robot_name: "robot-v1".to_string(),
            root_link_id: "base".to_string(),
            links: BTreeMap::from([
                (
                    "base".to_string(),
                    link(
                        "base",
                        vec![box_collision([1.0, 0.5, 0.2])],
                        Some(inertial(pose(0.0, 0.0, 0.0), 5.0, inertia(1.0, 2.0, 3.0))),
                    ),
                ),
                ("imu_mount".to_string(), link("imu_mount", Vec::new(), None)),
                (
                    "sensor_mount".to_string(),
                    link("sensor_mount", Vec::new(), None),
                ),
                (
                    "sensor_link".to_string(),
                    link(
                        "sensor_link",
                        vec![box_collision([0.02, 0.02, 0.01])],
                        Some(inertial(
                            pose(0.0, 0.0, 0.0),
                            0.02,
                            inertia(0.001, 0.001, 0.001),
                        )),
                    ),
                ),
            ]),
            joints: BTreeMap::from([
                (
                    "base_to_imu_mount".to_string(),
                    fixed_joint(
                        "base_to_imu_mount",
                        "base",
                        "imu_mount",
                        pose(0.05, 0.0, 0.0),
                    ),
                ),
                (
                    "imu_mount_to_sensor_mount".to_string(),
                    fixed_joint(
                        "imu_mount_to_sensor_mount",
                        "imu_mount",
                        "sensor_mount",
                        pose(0.05, 0.0, 0.0),
                    ),
                ),
                (
                    "sensor_mount_to_sensor_link".to_string(),
                    fixed_joint(
                        "sensor_mount_to_sensor_link",
                        "sensor_mount",
                        "sensor_link",
                        pose(0.0, 0.0, 0.0),
                    ),
                ),
            ]),
            contact_materials: BTreeMap::new(),
            runtime_components_for_joint: BTreeMap::new(),
            runtime_components_for_link: BTreeMap::new(),
            mounted_components_for_link: BTreeMap::new(),
            component_mesh_prefix: None,
        };

        let root = render_test_root(&scene)?;
        let Node::Solid(solid) = root else {
            panic!("expected root node to be a Solid");
        };
        let bounding_object = solid
            .bounding_object
            .expect("merged rigid body should keep a bounding object");
        let Node::Group(group) = bounding_object.unwrap_value().as_ref() else {
            panic!("expected merged collisions to become a Group");
        };
        assert_eq!(
            group
                .children
                .as_ref()
                .expect("group should contain collisions")
                .unwrap_value()
                .len(),
            2
        );

        let physics = solid
            .physics
            .expect("merged rigid body should keep physics");
        let Node::Physics(physics) = physics.unwrap_value().as_ref() else {
            panic!("expected merged rigid body physics");
        };
        assert!(
            (physics
                .mass
                .as_ref()
                .expect("mass should exist")
                .unwrap_value()
                - 5.02)
                .abs()
                < 1.0e-9
        );
        let center_of_mass = physics
            .center_of_mass
            .as_ref()
            .expect("center of mass must exist")
            .unwrap_value();
        assert!((center_of_mass.x - (0.02 * 0.1 / 5.02)).abs() < 1.0e-9);
        assert!(center_of_mass.y.abs() < 1.0e-12);
        assert!(center_of_mass.z.abs() < 1.0e-12);
        Ok(())
    }

    #[test]
    fn revolute_child_remains_separate_jointed_body() -> anyhow::Result<()> {
        let scene = WebotsSceneDescription {
            robot_name: "robot-v1".to_string(),
            root_link_id: "base".to_string(),
            links: BTreeMap::from([
                (
                    "base".to_string(),
                    link(
                        "base",
                        vec![box_collision([1.0, 0.5, 0.2])],
                        Some(inertial(pose(0.0, 0.0, 0.0), 5.0, inertia(1.0, 2.0, 3.0))),
                    ),
                ),
                (
                    "wheel".to_string(),
                    link(
                        "wheel",
                        vec![cylinder_collision(0.1, 0.05)],
                        Some(inertial(pose(0.0, 0.0, 0.0), 0.5, inertia(0.1, 0.1, 0.1))),
                    ),
                ),
            ]),
            joints: BTreeMap::from([(
                "base_to_wheel".to_string(),
                revolute_joint(
                    "base_to_wheel",
                    "base",
                    "wheel",
                    pose(0.3, 0.0, 0.0),
                    [0.0, 1.0, 0.0],
                    -1.0,
                    1.0,
                ),
            )]),
            contact_materials: BTreeMap::from([("wheel".to_string(), "caster_wheel".to_string())]),
            runtime_components_for_joint: BTreeMap::new(),
            runtime_components_for_link: BTreeMap::new(),
            mounted_components_for_link: BTreeMap::new(),
            component_mesh_prefix: None,
        };

        let root = render_test_root(&scene)?;
        let Node::Solid(solid) = root else {
            panic!("expected root node to be a Solid");
        };
        let children = solid.children.expect("root solid should contain children");
        assert!(
            children
                .unwrap_value()
                .iter()
                .any(|child| matches!(child, Node::HingeJoint(_)))
        );
        Ok(())
    }

    #[test]
    fn rendered_solid_emits_contact_material() -> anyhow::Result<()> {
        let scene = WebotsSceneDescription {
            robot_name: "robot-v1".to_string(),
            root_link_id: "base".to_string(),
            links: BTreeMap::from([(
                "base".to_string(),
                link(
                    "base",
                    vec![box_collision([1.0, 0.5, 0.2])],
                    Some(inertial(pose(0.0, 0.0, 0.0), 5.0, inertia(1.0, 2.0, 3.0))),
                ),
            )]),
            joints: BTreeMap::new(),
            contact_materials: BTreeMap::from([("base".to_string(), "rubber_wheel".to_string())]),
            runtime_components_for_joint: BTreeMap::new(),
            runtime_components_for_link: BTreeMap::new(),
            mounted_components_for_link: BTreeMap::new(),
            component_mesh_prefix: None,
        };

        let root = render_test_root(&scene)?;
        let Node::Solid(solid) = root else {
            panic!("expected root node to be a Solid");
        };
        assert_eq!(
            solid
                .contact_material
                .as_ref()
                .expect("contact material should be present")
                .unwrap_value(),
            "rubber_wheel"
        );
        Ok(())
    }
}
