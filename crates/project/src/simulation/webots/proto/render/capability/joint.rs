use webots_proto::r2025a::{Node, PositionSensor, RotationalMotor};
use webots_proto::types::ProtoField as WebotsField;

use crate::simulation::webots::proto::native_fields::{
    NativeValue, native_webots_fields_for_capability, native_webots_motor_fields,
};
use crate::simulation::webots::proto::scene::WebotsSceneDescription;
use phoxal::model::component::v0::capability::Capability as PhysicalCapability;
use phoxal::model::simulation::capability::Capability as SimulationCapability;

impl WebotsSceneDescription {
    pub fn render_joint_capabilities(&self, joint_id: &str) -> Vec<Node> {
        self.joint_bindings(joint_id)
            .into_iter()
            .filter_map(|binding| {
                self.render_joint_capability(
                    binding.capability_id.as_str(),
                    &binding.physical,
                    binding.simulation.as_ref(),
                )
            })
            .collect()
    }

    fn render_joint_capability(
        &self,
        capability_id: &str,
        physical: &PhysicalCapability,
        simulation: Option<&SimulationCapability>,
    ) -> Option<Node> {
        let max_torque = if let PhysicalCapability::Motor(phys) = physical {
            phys.max_torque_nm.unwrap_or(10.0)
        } else {
            10.0
        };
        let max_velocity = if let PhysicalCapability::Motor(phys) = physical {
            phys.max_velocity_radps.unwrap_or(10.0)
        } else {
            10.0
        };

        match (physical, simulation) {
            (PhysicalCapability::Motor(_), Some(SimulationCapability::Motor(config))) => {
                let mut motor = RotationalMotor::new(capability_id.to_string())
                    .with_max_torque(max_torque)
                    .with_max_velocity(max_velocity);
                motor.name = WebotsField::Is(Self::capability_name_field_name(capability_id));

                let native_fields = native_webots_motor_fields(config).ok()?;
                for assignment in native_fields.assignments {
                    match assignment.field_name.as_str() {
                        "acceleration" => {
                            if let NativeValue::Float(f) = assignment.value {
                                motor = motor.with_acceleration(f);
                            }
                        }
                        "controlPID" => {
                            if let NativeValue::Vec3(v) = assignment.value {
                                motor = motor.with_control_pid(Self::vec3(v));
                            }
                        }
                        _ => {}
                    }
                }
                Some(Node::RotationalMotor(motor))
            }
            (PhysicalCapability::Encoder(_), Some(sim @ SimulationCapability::Encoder(_))) => {
                let mut ps = PositionSensor::new(capability_id.to_string());
                ps.name = WebotsField::Is(Self::capability_name_field_name(capability_id));
                let native_fields = native_webots_fields_for_capability(sim);
                for assignment in native_fields.assignments {
                    match assignment.field_name.as_str() {
                        "resolution" => {
                            if let NativeValue::Float(f) = assignment.value {
                                ps = ps.with_resolution(f);
                            }
                        }
                        "noise" => {
                            if let NativeValue::Float(f) = assignment.value {
                                ps = ps.with_noise(f);
                            }
                        }
                        _ => {}
                    }
                }
                Some(Node::PositionSensor(ps))
            }
            _ => None,
        }
    }
}
