pub mod imu;
pub mod joint;
pub mod ranging;
pub mod vision;

use webots_proto::r2025a::Node;

use crate::simulation::webots::proto::scene::WebotsSceneDescription;
use phoxal_model::component::capability::Capability as PhysicalCapability;
use phoxal_model::simulation::Capability as SimulationCapability;

impl WebotsSceneDescription {
    pub fn render_link_capability(
        &self,
        capability_id: &str,
        physical: &PhysicalCapability,
        simulation: Option<&SimulationCapability>,
    ) -> Option<Node> {
        self.render_imu_family_capability(capability_id, physical, simulation)
            .or_else(|| self.render_vision_capability(capability_id, physical, simulation))
            .or_else(|| self.render_ranging_capability(capability_id, physical, simulation))
    }
}
