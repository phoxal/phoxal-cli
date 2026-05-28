use webots_proto::r2025a::{
    BoxNode, CadShape, Cylinder, Group, Mesh, Node, Pose, Shape, Sphere, Transform,
};

use crate::webots_staging::scene::WebotsSceneDescription;
use crate::webots_staging::support::paths::staged_mesh_path_from_urdf_filename;
use crate::webots_staging::types::StagedCollision;

impl WebotsSceneDescription {
    pub fn render_visual(
        &self,
        geometry: &urdf_rs::Geometry,
        origin: &nalgebra::Isometry3<f64>,
        mesh_url_prefix: &str,
    ) -> Node {
        let children = match geometry {
            urdf_rs::Geometry::Mesh { .. } => {
                vec![self.render_geometry_node(geometry, mesh_url_prefix)]
            }
            _ => vec![Node::Shape(Shape::new().with_geometry(Box::new(
                self.render_geometry_node(geometry, mesh_url_prefix),
            )))],
        };

        Node::Pose(
            Pose::new()
                .with_translation(Self::vec3([
                    origin.translation.vector.x,
                    origin.translation.vector.y,
                    origin.translation.vector.z,
                ]))
                .with_rotation(Self::rotation_from_isometry(origin))
                .with_children(children),
        )
    }

    pub fn render_assembly_bounding_object(
        &self,
        collisions: &[StagedCollision],
        mesh_url_prefix: &str,
    ) -> anyhow::Result<Option<Node>> {
        if collisions.is_empty() {
            return Ok(None);
        }
        let bounding_object = if collisions.len() == 1 {
            self.render_bounding_collision(&collisions[0], mesh_url_prefix)
        } else {
            Node::Group(
                Group::new().with_children(
                    collisions
                        .iter()
                        .map(|collision| self.render_bounding_collision(collision, mesh_url_prefix))
                        .collect::<Vec<_>>(),
                ),
            )
        };
        Ok(Some(bounding_object))
    }

    fn render_bounding_collision(
        &self,
        collision: &StagedCollision,
        mesh_url_prefix: &str,
    ) -> Node {
        Node::Transform(
            Transform::new()
                .with_translation(Self::vec3([
                    collision.origin.translation.vector.x,
                    collision.origin.translation.vector.y,
                    collision.origin.translation.vector.z,
                ]))
                .with_rotation(Self::rotation_from_isometry(&collision.origin))
                .with_children(vec![
                    self.render_bounding_geometry_node(&collision.geometry, mesh_url_prefix),
                ]),
        )
    }

    fn render_bounding_geometry_node(
        &self,
        geometry: &urdf_rs::Geometry,
        mesh_url_prefix: &str,
    ) -> Node {
        match geometry {
            urdf_rs::Geometry::Mesh { filename, .. } => {
                let normalized_staged_path = staged_mesh_path_from_urdf_filename(
                    filename,
                    self.component_mesh_prefix.as_deref(),
                );
                Node::Mesh(
                    Mesh::new()
                        .with_url(vec![format!("{mesh_url_prefix}/{normalized_staged_path}")]),
                )
            }
            _ => self.render_geometry_node(geometry, mesh_url_prefix),
        }
    }

    pub fn render_geometry_node(
        &self,
        geometry: &urdf_rs::Geometry,
        mesh_url_prefix: &str,
    ) -> Node {
        match geometry {
            urdf_rs::Geometry::Box { size } => {
                Node::Box(BoxNode::new().with_size(Self::vec3([size[0], size[1], size[2]])))
            }
            urdf_rs::Geometry::Cylinder { radius, length } => {
                Node::Cylinder(Cylinder::new().with_radius(*radius).with_height(*length))
            }
            urdf_rs::Geometry::Sphere { radius } => {
                Node::Sphere(Sphere::new().with_radius(*radius))
            }
            urdf_rs::Geometry::Mesh { filename, .. } => {
                let normalized_staged_path = staged_mesh_path_from_urdf_filename(
                    filename,
                    self.component_mesh_prefix.as_deref(),
                );
                Node::CadShape(
                    CadShape::new()
                        .with_url(vec![format!("{mesh_url_prefix}/{normalized_staged_path}")]),
                )
            }
            urdf_rs::Geometry::Capsule { radius, length } => {
                Node::Cylinder(Cylinder::new().with_radius(*radius).with_height(*length))
            }
        }
    }
}
