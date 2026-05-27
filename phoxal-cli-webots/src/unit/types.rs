use webots_proto::r2025a::Node;

use crate::unit::support::inertia::MassPropertiesAccumulator;

#[derive(Debug, Clone, Default)]
pub struct FixedSubtreeRender {
    pub children: Vec<Node>,
    pub collisions: Vec<StagedCollision>,
    pub mass_properties: MassPropertiesAccumulator,
}

impl FixedSubtreeRender {
    pub fn extend(&mut self, other: Self) {
        self.children.extend(other.children);
        self.collisions.extend(other.collisions);
        self.mass_properties.extend(other.mass_properties);
    }
}

#[derive(Debug, Clone)]
pub struct StagedCollision {
    pub origin: nalgebra::Isometry3<f64>,
    pub geometry: urdf_rs::Geometry,
}
