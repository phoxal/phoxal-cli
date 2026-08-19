use webots_proto::r2025a::{
    BoxNode, CadShape, Cylinder, Group, Mesh, Node, Pose, Shape, Sphere, Transform,
};

use crate::simulation::webots::proto::scene::WebotsSceneDescription;
use crate::simulation::webots::proto::support::paths::staged_geometry_path;
use crate::simulation::webots::proto::types::StagedCollision;
use phoxal::model::structure::Geometry;

impl WebotsSceneDescription {
    pub fn render_visual(
        &self,
        geometry: &Geometry,
        origin: &nalgebra::Isometry3<f64>,
        mesh_url_prefix: &str,
    ) -> Node {
        let children = match geometry {
            Geometry::Mesh { .. } => {
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

    fn render_bounding_geometry_node(&self, geometry: &Geometry, mesh_url_prefix: &str) -> Node {
        match geometry {
            Geometry::Mesh { asset, .. } => {
                let normalized_staged_path = staged_geometry_path(asset);
                Node::Mesh(
                    Mesh::new()
                        .with_url(vec![format!("{mesh_url_prefix}/{normalized_staged_path}")]),
                )
            }
            _ => self.render_geometry_node(geometry, mesh_url_prefix),
        }
    }

    pub fn render_geometry_node(&self, geometry: &Geometry, mesh_url_prefix: &str) -> Node {
        match geometry {
            Geometry::Box { size } => {
                Node::Box(BoxNode::new().with_size(Self::vec3([size[0], size[1], size[2]])))
            }
            Geometry::Cylinder { radius, length } => {
                Node::Cylinder(Cylinder::new().with_radius(*radius).with_height(*length))
            }
            Geometry::Sphere { radius } => Node::Sphere(Sphere::new().with_radius(*radius)),
            Geometry::Mesh { asset, .. } => {
                let normalized_staged_path = staged_geometry_path(asset);
                Node::CadShape(
                    CadShape::new()
                        .with_url(vec![format!("{mesh_url_prefix}/{normalized_staged_path}")]),
                )
            }
            Geometry::Capsule { radius, length } => {
                Node::Cylinder(Cylinder::new().with_radius(*radius).with_height(*length))
            }
        }
    }
}
