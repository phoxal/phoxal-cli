use phoxal_model::component::capability::{
    CameraMode, Capability as PhysicalCapability, EncoderType, LidarOutput,
};
use phoxal_model::simulation::capability::{ActuatorType, Capability as SimulationCapability};

use crate::simulation::webots::proto::scene::WebotsSceneDescription;

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
                SimulationCapability::Motor(config) => {
                    Some(actuator_type_name(config.actuator_type))
                }
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
            "encoder_type": encoder_type_name(config.encoder_type),
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
            value["mode"] = serde_json::json!(camera_mode_name(config.mode));
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
            "lidar_output": lidar_output_name(config.output),
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

const fn actuator_type_name(value: ActuatorType) -> &'static str {
    match value {
        ActuatorType::Velocity => "velocity",
        ActuatorType::Position => "position",
        ActuatorType::Torque => "torque",
    }
}

const fn encoder_type_name(value: EncoderType) -> &'static str {
    match value {
        EncoderType::Incremental => "incremental",
        EncoderType::Absolute => "absolute",
    }
}

const fn camera_mode_name(value: CameraMode) -> &'static str {
    match value {
        CameraMode::Mono => "mono",
        CameraMode::Rgb => "rgb",
    }
}

const fn lidar_output_name(value: LidarOutput) -> &'static str {
    match value {
        LidarOutput::Ranges => "ranges",
        LidarOutput::Points => "points",
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
    use super::*;
    use phoxal_model::component::capability::{
        Camera, Encoder, Lidar, Motor, MotorCommand, StructuralTarget,
    };
    use phoxal_model::simulation::capability as simulation;

    fn target() -> StructuralTarget {
        StructuralTarget::Link {
            id: "sensor".to_string(),
        }
    }

    fn emitted_controller(
        capability_id: &str,
        physical: &PhysicalCapability,
        simulation: Option<&SimulationCapability>,
    ) -> serde_json::Value {
        let comments =
            runtime_metadata_comments_for_capability(capability_id, physical, simulation);
        assert_eq!(comments.len(), 1);
        let prefix = format!("# rf: capability={capability_id} controller=");
        serde_json::from_str(
            comments[0]
                .strip_prefix(&prefix)
                .expect("runtime metadata comment should retain its wire prefix"),
        )
        .expect("runtime metadata controller payload should be JSON")
    }

    #[test]
    fn canonical_enum_names_retain_the_controller_wire_vocabulary() {
        assert_eq!(actuator_type_name(ActuatorType::Velocity), "velocity");
        assert_eq!(actuator_type_name(ActuatorType::Position), "position");
        assert_eq!(actuator_type_name(ActuatorType::Torque), "torque");
        assert_eq!(encoder_type_name(EncoderType::Incremental), "incremental");
        assert_eq!(encoder_type_name(EncoderType::Absolute), "absolute");
        assert_eq!(camera_mode_name(CameraMode::Mono), "mono");
        assert_eq!(camera_mode_name(CameraMode::Rgb), "rgb");
        assert_eq!(lidar_output_name(LidarOutput::Ranges), "ranges");
        assert_eq!(lidar_output_name(LidarOutput::Points), "points");
    }

    #[test]
    fn canonical_capabilities_emit_the_existing_controller_metadata_shape() {
        let motor = PhysicalCapability::Motor(Motor {
            target: target(),
            command: MotorCommand::Velocity,
            gear_ratio: 2.0,
            max_torque_nm: None,
            max_velocity_radps: None,
        });
        let motor_simulation = SimulationCapability::Motor(simulation::Motor {
            actuator_type: ActuatorType::Torque,
            ..simulation::Motor::default()
        });
        let motor_json = emitted_controller("drive", &motor, Some(&motor_simulation));
        assert_eq!(motor_json["kind"], "motor");
        assert_eq!(motor_json["actuator_type"], "torque");
        assert_eq!(motor_json["gear_ratio"], 2.0);

        let encoder = PhysicalCapability::Encoder(Encoder {
            target: target(),
            publish_rate_hz: 50.0,
            gear_ratio: 1.0,
            encoder_type: EncoderType::Absolute,
            counts_per_revolution: 4096,
        });
        let encoder_json = emitted_controller("position", &encoder, None);
        assert_eq!(encoder_json["kind"], "encoder");
        assert_eq!(encoder_json["encoder_type"], "absolute");
        assert_eq!(encoder_json["counts_per_revolution"], 4096);
        assert!(encoder_json["sampling_period_hz"].is_null());

        let camera = PhysicalCapability::Camera(Camera {
            target: target(),
            mode: CameraMode::Rgb,
            publish_rate_hz: 30.0,
            width_px: 640,
            height_px: 480,
            field_of_view_rad: None,
        });
        let camera_json = emitted_controller("front_camera", &camera, None);
        assert_eq!(camera_json["kind"], "camera");
        assert_eq!(camera_json["mode"], "rgb");

        let lidar = PhysicalCapability::Lidar(Lidar {
            target: target(),
            publish_rate_hz: 10.0,
            output: LidarOutput::Points,
            min_range_m: None,
            max_range_m: None,
            horizontal_fov_rad: None,
            horizontal_resolution_rad: None,
            vertical_fov_rad: None,
            vertical_resolution_rad: None,
        });
        let lidar_json = emitted_controller("lidar", &lidar, None);
        assert_eq!(lidar_json["kind"], "lidar");
        assert_eq!(lidar_json["lidar_output"], "points");
    }
}
