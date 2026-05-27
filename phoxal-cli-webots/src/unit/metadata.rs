use phoxal_utils_component::v1::capability::Capability as PhysicalCapability;
use phoxal_utils_simulation::capability::Capability as SimulationCapability;

use crate::unit::scene::WebotsSceneDescription;

pub fn runtime_metadata_comments(scene: &WebotsSceneDescription) -> String {
    let mut comments = String::new();
    for bindings in scene
        .runtime_components_for_joint
        .values()
        .chain(scene.runtime_components_for_link.values())
    {
        for binding in bindings {
            for comment in runtime_metadata_comments_for_capability(
                &binding.capability_id,
                &binding.physical,
                binding.simulation.as_ref(),
            ) {
                comments.push_str(&comment);
                comments.push('\n');
            }
        }
    }
    comments
}

pub fn runtime_metadata_comments_for_capability(
    capability_id: &str,
    physical: &PhysicalCapability,
    simulation: Option<&SimulationCapability>,
) -> Vec<String> {
    let controller = controller_metadata_json(physical, simulation);
    vec![format!(
        "# rf: capability={capability_id} controller={}",
        serde_json::to_string(&controller).expect("controller metadata should serialize")
    )]
}

fn controller_metadata_json(
    physical: &PhysicalCapability,
    simulation: Option<&SimulationCapability>,
) -> serde_json::Value {
    match physical {
        PhysicalCapability::Motor(config) => serde_json::json!({
            "kind": "motor",
            "gear_ratio": config.gear_ratio,
            "actuator_type": simulation.and_then(|simulation| match simulation {
                SimulationCapability::Motor(config) => Some(config.actuator_type),
                _ => None,
            }),
        }),
        PhysicalCapability::Encoder(config) => serde_json::json!({
            "kind": "encoder",
            "publish_rate_hz": config.publish_rate_hz,
            "sampling_period_hz": simulation.and_then(|simulation| match simulation {
                SimulationCapability::Encoder(config) => Some(config.sampling_period_hz),
                _ => None,
            }),
            "gear_ratio": config.gear_ratio,
            "counts_per_revolution": config.counts_per_revolution,
            "encoder_type": config.encoder_type,
        }),
        PhysicalCapability::Accelerometer(config) => controller_rate_metadata(
            "accelerometer",
            config.publish_rate_hz,
            simulation.and_then(|simulation| match simulation {
                SimulationCapability::Accelerometer(config) => Some(config.sampling_period_hz),
                _ => None,
            }),
        ),
        PhysicalCapability::Gyroscope(config) => controller_rate_metadata(
            "gyroscope",
            config.publish_rate_hz,
            simulation.and_then(|simulation| match simulation {
                SimulationCapability::Gyroscope(config) => Some(config.sampling_period_hz),
                _ => None,
            }),
        ),
        PhysicalCapability::Magnetometer(config) => controller_rate_metadata(
            "magnetometer",
            config.publish_rate_hz,
            simulation.and_then(|simulation| match simulation {
                SimulationCapability::Magnetometer(config) => Some(config.sampling_period_hz),
                _ => None,
            }),
        ),
        PhysicalCapability::Imu(config) => controller_rate_metadata(
            "imu",
            config.publish_rate_hz,
            simulation.and_then(|simulation| match simulation {
                SimulationCapability::Imu(config) => Some(config.sampling_period_hz),
                _ => None,
            }),
        ),
        PhysicalCapability::Gnss(config) => controller_rate_metadata(
            "gnss",
            config.publish_rate_hz,
            simulation.and_then(|simulation| match simulation {
                SimulationCapability::Gnss(config) => Some(config.sampling_period_hz),
                _ => None,
            }),
        ),
        PhysicalCapability::Camera(config) => {
            let mut value = controller_rate_metadata(
                "camera",
                config.publish_rate_hz,
                simulation.and_then(|simulation| match simulation {
                    SimulationCapability::Camera(config) => Some(config.sampling_period_hz),
                    _ => None,
                }),
            );
            value["mode"] = serde_json::json!(config.mode);
            value
        }
        PhysicalCapability::Depth(config) => serde_json::json!({
            "kind": "depth",
            "publish_rate_hz": config.publish_rate_hz,
            "sampling_period_hz": simulation.and_then(|simulation| match simulation {
                SimulationCapability::Depth(config) => Some(config.sampling_period_hz),
                _ => None,
            }),
        }),
        PhysicalCapability::EmergencyStop(_) => serde_json::json!({
            "kind": "emergency_stop",
        }),
        PhysicalCapability::Range(config) => serde_json::json!({
            "kind": "range",
            "publish_rate_hz": config.publish_rate_hz,
            "sampling_period_hz": simulation.and_then(|simulation| match simulation {
                SimulationCapability::Range(config) => Some(config.sampling_period_hz),
                _ => None,
            }),
        }),
        PhysicalCapability::Lidar(config) => serde_json::json!({
            "kind": "lidar",
            "publish_rate_hz": config.publish_rate_hz,
            "sampling_period_hz": simulation.and_then(|simulation| match simulation {
                SimulationCapability::Lidar(config) => Some(config.sampling_period_hz),
                _ => None,
            }),
            "lidar_output": config.output,
        }),
        PhysicalCapability::Mmwave(config) => controller_rate_metadata(
            "mmwave",
            config.publish_rate_hz,
            simulation.and_then(|simulation| match simulation {
                SimulationCapability::Mmwave(config) => Some(config.sampling_period_hz),
                _ => None,
            }),
        ),
        PhysicalCapability::Microphone(config) => controller_rate_metadata(
            "microphone",
            config.publish_rate_hz,
            simulation.and_then(|simulation| match simulation {
                SimulationCapability::Microphone(config) => Some(config.sampling_period_hz),
                _ => None,
            }),
        ),
        PhysicalCapability::Speaker(_) => serde_json::json!({
            "kind": "speaker",
        }),
        PhysicalCapability::Battery(config) => serde_json::json!({
            "kind": "battery",
            "publish_rate_hz": config.publish_rate_hz,
            "voltage_v": config.voltage_v,
            "capacity_ah": config.capacity_ah,
        }),
        PhysicalCapability::Led(_) => serde_json::json!({
            "kind": "led",
        }),
    }
}

fn controller_rate_metadata(
    kind: &'static str,
    publish_rate_hz: f64,
    sampling_period_hz: Option<f64>,
) -> serde_json::Value {
    serde_json::json!({
        "kind": kind,
        "publish_rate_hz": publish_rate_hz,
        "sampling_period_hz": sampling_period_hz,
    })
}

#[cfg(test)]
mod tests {
    use super::runtime_metadata_comments_for_capability;
    use phoxal_utils_component::v1::capability::Capability as PhysicalCapability;
    use phoxal_utils_simulation::capability::Capability as SimulationCapability;

    #[test]
    fn motor_correct_sampling_period_logic() {
        let physical = PhysicalCapability::Motor(phoxal_utils_component::v1::capability::Motor {
            target: phoxal_utils_component::v1::capability::StructuralTarget::Joint {
                id: "joint".into(),
            },
            command: phoxal_utils_component::v1::capability::MotorCommand::Velocity,
            gear_ratio: 1.0,
            max_torque_nm: None,
            max_velocity_radps: None,
        });
        let sim = SimulationCapability::Motor(phoxal_utils_simulation::capability::Motor {
            actuator_type: phoxal_utils_simulation::capability::ActuatorType::Torque,
            ..Default::default()
        });
        let comments = runtime_metadata_comments_for_capability("m0", &physical, Some(&sim));
        assert_eq!(comments.len(), 1);
        assert!(comments[0].contains("controller={"));
        assert!(comments[0].contains("\"actuator_type\":\"torque\""));
    }

    #[test]
    fn runtime_metadata_values_are_unquoted() {
        let physical =
            PhysicalCapability::Encoder(phoxal_utils_component::v1::capability::Encoder {
                target: phoxal_utils_component::v1::capability::StructuralTarget::Joint {
                    id: "joint".into(),
                },
                publish_rate_hz: 50.0,
                gear_ratio: 1.0,
                encoder_type: phoxal_utils_component::v1::capability::EncoderType::Incremental,
                counts_per_revolution: 1024,
            });
        let sim = SimulationCapability::Encoder(phoxal_utils_simulation::capability::Encoder {
            sampling_period_hz: 50.0,
            ..Default::default()
        });
        let comments = runtime_metadata_comments_for_capability("enc", &physical, Some(&sim));
        assert_eq!(comments.len(), 1);
        assert!(comments[0].contains("\"counts_per_revolution\":1024"));
        assert!(comments[0].contains("\"sampling_period_hz\":50.0"));
    }

    #[test]
    fn capability_local_id_is_used() {
        let physical = PhysicalCapability::Led(phoxal_utils_component::v1::capability::Led {
            target: phoxal_utils_component::v1::capability::StructuralTarget::Link {
                id: "base".into(),
            },
        });
        let comments = runtime_metadata_comments_for_capability("local_name", &physical, None);
        assert!(comments[0].contains("capability=local_name"));
        assert!(comments[0].contains("controller={"));
    }
}
