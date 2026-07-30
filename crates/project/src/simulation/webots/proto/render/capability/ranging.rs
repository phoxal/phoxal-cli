use webots_proto::r2025a::{DistanceSensor, DistanceSensorType, Lidar, Node, Radar};
use webots_proto::types::ProtoField as WebotsField;

use crate::simulation::webots::proto::native_fields::{
    NativeValue, native_webots_fields_for_capability,
};
use crate::simulation::webots::proto::scene::WebotsSceneDescription;
use phoxal::model::component::capability::{Capability as PhysicalCapability, LidarOutput};
use phoxal::model::simulation::capability::Capability as SimulationCapability;

impl WebotsSceneDescription {
    pub fn render_ranging_capability(
        &self,
        capability_id: &str,
        physical: &PhysicalCapability,
        simulation: Option<&SimulationCapability>,
    ) -> Option<Node> {
        match (physical, simulation) {
            (PhysicalCapability::Range(phys), Some(SimulationCapability::Range(config))) => {
                let mut distance_sensor = DistanceSensor::new(capability_id.to_string())
                    .with_type(DistanceSensorType::Laser)
                    .with_aperture(phys.field_of_view_rad)
                    .with_resolution(config.resolution.unwrap_or(0.001));
                distance_sensor.name =
                    WebotsField::Is(Self::capability_name_field_name(capability_id));
                distance_sensor.model = Some(capability_id.to_string().into());
                distance_sensor.description =
                    Some(capability_id.replace('_', " ").replace('.', " / ").into());
                distance_sensor =
                    distance_sensor.with_lookup_table(Self::lookup_table_from_native(&[
                        [
                            phys.min_range_m,
                            phys.min_range_m,
                            config.noise.unwrap_or_default(),
                        ],
                        [
                            phys.max_range_m,
                            phys.max_range_m,
                            config.noise.unwrap_or_default(),
                        ],
                    ]));
                Some(Node::DistanceSensor(distance_sensor))
            }
            (
                PhysicalCapability::Lidar(phys_config),
                Some(sim @ SimulationCapability::Lidar(_)),
            ) => {
                let horizontal_fov = phys_config.horizontal_fov_rad.unwrap_or(
                    if matches!(phys_config.output, LidarOutput::Ranges) {
                        std::f64::consts::FRAC_PI_2
                    } else {
                        std::f64::consts::TAU
                    },
                );
                let horizontal_res = phys_config.horizontal_resolution_rad.unwrap_or(0.01);
                let horizontal_resolution = (horizontal_fov / horizontal_res).round() as i32;
                let horizontal_resolution = horizontal_resolution.max(1);

                let mut lidar = Lidar::new(capability_id.to_string())
                    .with_field_of_view(horizontal_fov)
                    .with_horizontal_resolution(horizontal_resolution)
                    .with_min_range(phys_config.min_range_m.unwrap_or(0.01))
                    .with_max_range(phys_config.max_range_m.unwrap_or(10.0));
                lidar.name = WebotsField::Is(Self::capability_name_field_name(capability_id));

                if matches!(phys_config.output, LidarOutput::Points) {
                    let vertical_fov = phys_config.vertical_fov_rad.unwrap_or(0.261799);
                    let vertical_res = phys_config.vertical_resolution_rad.unwrap_or(0.01);
                    let number_of_layers = ((vertical_fov / vertical_res).round() as i32).max(1);
                    lidar = lidar
                        .with_vertical_field_of_view(vertical_fov)
                        .with_number_of_layers(number_of_layers);
                } else {
                    lidar = lidar.with_number_of_layers(1);
                }

                let native_fields = native_webots_fields_for_capability(sim);
                for assignment in native_fields.assignments {
                    match assignment.field_name.as_str() {
                        "resolution" => {
                            if let NativeValue::Float(f) = assignment.value {
                                lidar = lidar.with_resolution(f);
                            }
                        }
                        "noise" => {
                            if let NativeValue::Float(f) = assignment.value {
                                lidar = lidar.with_noise(f);
                            }
                        }
                        _ => {}
                    }
                }
                Some(Node::Lidar(lidar))
            }
            (PhysicalCapability::Mmwave(_), Some(SimulationCapability::Mmwave(_))) => {
                let mut radar = Radar::new(capability_id.to_string());
                radar.name = WebotsField::Is(Self::capability_name_field_name(capability_id));
                Some(Node::Radar(radar))
            }
            _ => None,
        }
    }
}
