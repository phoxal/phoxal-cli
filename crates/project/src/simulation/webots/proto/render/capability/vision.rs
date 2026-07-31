use webots_proto::r2025a::{Camera as WebotsCamera, Node, RangeFinder};
use webots_proto::types::ProtoField as WebotsField;

use crate::simulation::webots::proto::native_fields::{
    NativeValue, native_webots_fields_for_capability,
};
use crate::simulation::webots::proto::scene::WebotsSceneDescription;
use phoxal_model::component::capability::Capability as PhysicalCapability;
use phoxal_model::simulation::capability::Capability as SimulationCapability;

impl WebotsSceneDescription {
    pub fn render_vision_capability(
        &self,
        capability_id: &str,
        physical: &PhysicalCapability,
        simulation: Option<&SimulationCapability>,
    ) -> Option<Node> {
        match (physical, simulation) {
            (PhysicalCapability::Camera(phys), Some(sim @ SimulationCapability::Camera(_))) => {
                let mut camera = WebotsCamera::new(capability_id.to_string())
                    .with_width(phys.width_px as i32)
                    .with_height(phys.height_px as i32)
                    .with_field_of_view(phys.field_of_view_rad.unwrap_or(1.0));
                camera.name = WebotsField::Is(Self::capability_name_field_name(capability_id));
                camera.model = Some(capability_id.to_string().into());
                camera.description =
                    Some(capability_id.replace('_', " ").replace('.', " / ").into());

                let native_fields = native_webots_fields_for_capability(sim);
                for assignment in native_fields.assignments {
                    match assignment.field_name.as_str() {
                        "projection" => {
                            if let NativeValue::String(value) = assignment.value {
                                let projection = match value.as_str() {
                                    "planar" => Some(webots_proto::r2025a::Projection::Planar),
                                    "cylindrical" => {
                                        Some(webots_proto::r2025a::Projection::Cylindrical)
                                    }
                                    "spherical" => {
                                        Some(webots_proto::r2025a::Projection::Spherical)
                                    }
                                    _ => None,
                                };
                                if let Some(projection) = projection {
                                    camera = camera.with_projection(projection);
                                }
                            }
                        }
                        "near" => {
                            if let NativeValue::Float(f) = assignment.value {
                                camera = camera.with_near(f);
                            }
                        }
                        "far" => {
                            if let NativeValue::Float(f) = assignment.value {
                                camera = camera.with_far(f);
                            }
                        }
                        "exposure" => {
                            if let NativeValue::Float(f) = assignment.value {
                                camera = camera.with_exposure(f);
                            }
                        }
                        "antiAliasing" => {
                            if let NativeValue::Bool(value) = assignment.value {
                                camera = camera.with_anti_aliasing(value);
                            }
                        }
                        "ambientOcclusionRadius" => {
                            if let NativeValue::Float(f) = assignment.value {
                                camera = camera.with_ambient_occlusion_radius(f);
                            }
                        }
                        "bloomThreshold" => {
                            if let NativeValue::Float(f) = assignment.value {
                                camera = camera.with_bloom_threshold(f);
                            }
                        }
                        "noise" => {
                            if let NativeValue::Float(f) = assignment.value {
                                camera = camera.with_noise(f);
                            }
                        }
                        "motionBlur" => {
                            if let NativeValue::Float(f) = assignment.value {
                                camera = camera.with_motion_blur(f);
                            }
                        }
                        "noiseMaskUrl" => {
                            if let NativeValue::String(value) = assignment.value {
                                camera = camera.with_noise_mask_url(value);
                            }
                        }
                        _ => {}
                    }
                }
                Some(Node::Camera(camera))
            }
            (PhysicalCapability::Depth(phys), Some(sim @ SimulationCapability::Depth(_))) => {
                let mut range_finder = RangeFinder::new(capability_id.to_string())
                    .with_width(phys.width_px as i32)
                    .with_height(phys.height_px as i32)
                    .with_field_of_view(phys.field_of_view_rad.unwrap_or(1.0))
                    .with_near(phys.min_range_m.unwrap_or(0.01))
                    .with_min_range(phys.min_range_m.unwrap_or(0.01))
                    .with_max_range(phys.max_range_m.unwrap_or(10.0));
                range_finder.name =
                    WebotsField::Is(Self::capability_name_field_name(capability_id));
                range_finder.model = Some(capability_id.to_string().into());
                range_finder.description =
                    Some(capability_id.replace('_', " ").replace('.', " / ").into());
                let native_fields = native_webots_fields_for_capability(sim);
                for assignment in native_fields.assignments {
                    match assignment.field_name.as_str() {
                        "resolution" => {
                            if let NativeValue::Float(f) = assignment.value {
                                range_finder = range_finder.with_resolution(f);
                            }
                        }
                        "noise" => {
                            if let NativeValue::Float(f) = assignment.value {
                                range_finder = range_finder.with_noise(f);
                            }
                        }
                        "motionBlur" => {
                            if let NativeValue::Float(f) = assignment.value {
                                range_finder = range_finder.with_motion_blur(f);
                            }
                        }
                        _ => {}
                    }
                }
                Some(Node::RangeFinder(range_finder))
            }
            _ => None,
        }
    }
}
