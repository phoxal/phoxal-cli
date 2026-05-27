use anyhow::{Context, Result, bail};
use phoxal_utils_component::v1::CapabilityRef;
use phoxal_utils_component::v1::capability::{Capability, MotorCommand};
use phoxal_utils_robot::v1::{self as model_v1, ModelV1};
use phoxal_utils_structure::Structure;
use std::collections::{BTreeMap, BTreeSet};

pub fn validate_model_components<F>(
    model: &ModelV1,
    structure: &Structure,
    mut load_component: F,
    component_has_driver: impl Fn(&str) -> bool,
    allow_driverless_capabilities: bool,
) -> Result<()>
where
    F: FnMut(&str) -> Result<phoxal_utils_component::Component>,
{
    let model_link_ids = structure
        .links
        .iter()
        .map(|link| link.name.as_str())
        .collect::<BTreeSet<_>>();
    let mut components_by_instance = BTreeMap::new();
    let mut components_by_type = BTreeMap::new();
    let mut resolved_capability_ids = BTreeSet::new();

    for (component_id, component_instance) in &model.components {
        if !model_link_ids.contains(component_instance.mount_link.as_str()) {
            bail!(
                "component '{}' references missing mount_link '{}'",
                component_id,
                component_instance.mount_link
            );
        }

        let component = load_component(&component_instance.component).with_context(|| {
            format!(
                "failed to load component type '{}'",
                component_instance.component
            )
        })?;
        let component = component
            .as_v1()
            .context("source graph only supports component.yaml version v1")?
            .clone();

        if component_instance.driver.is_some() {
            let has_driver_package = component_has_driver(&component_instance.component);
            if !has_driver_package {
                bail!(
                    "component '{}' provides driver config but component type '{}' has no components/{}/Cargo.toml",
                    component_id,
                    component_instance.component,
                    component_instance.component
                );
            }
            if component.capabilities.is_empty() {
                bail!(
                    "component '{}' provides driver config but component type '{}' defines no capabilities",
                    component_id,
                    component_instance.component
                );
            }
        } else if !allow_driverless_capabilities && !component.capabilities.is_empty() {
            bail!(
                "component '{}' of type '{}' defines capabilities but provides no driver config; \
                 real models require a driver (only fixtures may omit it)",
                component_id,
                component_instance.component
            );
        }
        // Driver config is optional only for fixture robots: no driver means no
        // hardware process for this component instance, so simulation may provide
        // capability data directly.

        for capability_id in component_instance.parameters.keys() {
            if !component.capabilities.contains_key(capability_id) {
                bail!(
                    "component '{}' parameters reference unknown capability '{}'",
                    component_id,
                    capability_id
                );
            }
        }

        for capability_id in component.capabilities.keys() {
            resolved_capability_ids.insert(CapabilityRef::new(component_id, capability_id));
        }
        components_by_type.insert(component_instance.component.clone(), component.clone());
        components_by_instance.insert(component_id.clone(), component);
    }

    let mut validation_errors = Vec::new();

    for capability_ref in motion_capability_refs(&model.motion.kinematic) {
        if !resolved_capability_ids.contains(&capability_ref) {
            validation_errors.push(format!(
                "motion references missing capability '{}'",
                capability_ref
            ));
        }
    }

    if let Err(error) =
        phoxal_utils_robot::v1::role_resolution::resolve_roles(model, &components_by_type)
    {
        validation_errors.push(format!("{error:#}"));
    }
    for actuator in drive_actuator_refs(&model.motion.kinematic) {
        validate_runtime_capability_kind(
            &components_by_instance,
            actuator.capability,
            &actuator.field,
            actuator.predicate,
            actuator.expected,
            &mut validation_errors,
        );
    }

    if validation_errors.is_empty() {
        Ok(())
    } else {
        bail!(
            "Robot source graph errors:\n{}",
            validation_errors.join("\n")
        )
    }
}

fn validate_runtime_capability_kind(
    components_by_instance: &BTreeMap<String, phoxal_utils_component::v1::Component>,
    capability_ref: &CapabilityRef,
    field: &str,
    predicate: fn(&Capability) -> bool,
    expected: &str,
    validation_errors: &mut Vec<String>,
) {
    match components_by_instance
        .get(&capability_ref.component_id)
        .and_then(|component| component.capabilities.get(&capability_ref.capability_id))
    {
        Some(capability) if predicate(capability) => {}
        Some(_) => validation_errors.push(format!(
            "{field} '{}' must reference a {expected} capability",
            capability_ref
        )),
        None => validation_errors.push(format!(
            "{field} references missing capability '{}'",
            capability_ref
        )),
    }
}

fn motion_capability_refs(kinematic: &model_v1::KinematicConfig) -> Vec<CapabilityRef> {
    match kinematic {
        model_v1::KinematicConfig::Differential {
            left_actuators,
            right_actuators,
            left_encoders,
            right_encoders,
            ..
        } => left_actuators
            .iter()
            .chain(right_actuators.iter())
            .chain(left_encoders.iter())
            .chain(right_encoders.iter())
            .cloned()
            .collect(),
        model_v1::KinematicConfig::Mecanum {
            front_left_actuator,
            front_right_actuator,
            rear_left_actuator,
            rear_right_actuator,
            ..
        } => vec![
            front_left_actuator.clone(),
            front_right_actuator.clone(),
            rear_left_actuator.clone(),
            rear_right_actuator.clone(),
        ],
        model_v1::KinematicConfig::Ackermann {
            steering_actuator,
            drive_actuator,
            steering_encoder,
            drive_encoder,
            ..
        } => {
            let mut refs = vec![steering_actuator.clone(), drive_actuator.clone()];
            if let Some(capability_ref) = steering_encoder {
                refs.push(capability_ref.clone());
            }
            if let Some(capability_ref) = drive_encoder {
                refs.push(capability_ref.clone());
            }
            refs
        }
        model_v1::KinematicConfig::Omnidirectional {
            actuators,
            encoders,
        } => actuators.iter().chain(encoders.iter()).cloned().collect(),
    }
}

fn drive_actuator_refs(kinematic: &model_v1::KinematicConfig) -> Vec<DriveActuatorRef<'_>> {
    match kinematic {
        model_v1::KinematicConfig::Differential {
            left_actuators,
            right_actuators,
            ..
        } => {
            let mut refs = Vec::new();
            for (index, capability_ref) in left_actuators.iter().enumerate() {
                refs.push(DriveActuatorRef::velocity(
                    format!("motion.kinematic.left_actuators[{index}]"),
                    capability_ref,
                ));
            }
            for (index, capability_ref) in right_actuators.iter().enumerate() {
                refs.push(DriveActuatorRef::velocity(
                    format!("motion.kinematic.right_actuators[{index}]"),
                    capability_ref,
                ));
            }
            refs
        }
        model_v1::KinematicConfig::Mecanum {
            front_left_actuator,
            front_right_actuator,
            rear_left_actuator,
            rear_right_actuator,
            ..
        } => vec![
            DriveActuatorRef::velocity("motion.kinematic.front_left_actuator", front_left_actuator),
            DriveActuatorRef::velocity(
                "motion.kinematic.front_right_actuator",
                front_right_actuator,
            ),
            DriveActuatorRef::velocity("motion.kinematic.rear_left_actuator", rear_left_actuator),
            DriveActuatorRef::velocity("motion.kinematic.rear_right_actuator", rear_right_actuator),
        ],
        model_v1::KinematicConfig::Ackermann {
            steering_actuator,
            drive_actuator,
            ..
        } => vec![
            DriveActuatorRef::motor("motion.kinematic.steering_actuator", steering_actuator),
            DriveActuatorRef::velocity("motion.kinematic.drive_actuator", drive_actuator),
        ],
        model_v1::KinematicConfig::Omnidirectional { actuators, .. } => actuators
            .iter()
            .map(|actuator| DriveActuatorRef::velocity("motion.kinematic.actuators", actuator))
            .collect(),
    }
}

struct DriveActuatorRef<'a> {
    field: String,
    capability: &'a CapabilityRef,
    expected: &'static str,
    predicate: fn(&Capability) -> bool,
}

impl<'a> DriveActuatorRef<'a> {
    fn velocity(field: impl Into<String>, capability: &'a CapabilityRef) -> Self {
        Self {
            field: field.into(),
            capability,
            expected: "velocity motor",
            predicate: is_velocity_motor_capability,
        }
    }

    fn motor(field: impl Into<String>, capability: &'a CapabilityRef) -> Self {
        Self {
            field: field.into(),
            capability,
            expected: "motor",
            predicate: is_motor_capability,
        }
    }
}

fn is_motor_capability(capability: &Capability) -> bool {
    matches!(capability, Capability::Motor(_))
}

fn is_velocity_motor_capability(capability: &Capability) -> bool {
    matches!(
        capability,
        Capability::Motor(motor) if motor.command == MotorCommand::Velocity
    )
}

#[cfg(test)]
mod tests {
    use super::validate_model_components;
    use phoxal_utils_robot::Model;
    use phoxal_utils_structure::Structure;

    #[test]
    fn real_model_rejects_capability_component_without_driver() -> anyhow::Result<()> {
        let model = model_with_driver_block("")?;
        let structure = structure_with_fixture_mount()?;

        let error = validate_model_components(
            &model,
            &structure,
            |_| drive_motor_component(),
            |_| false,
            false,
        )
        .expect_err("real model capability component without a driver should fail");

        assert!(
            error.to_string().contains(
                "component 'fixture_drive' of type 'drive_motor' defines capabilities but provides no driver config; real models require a driver (only fixtures may omit it)"
            ),
            "{error:#}"
        );
        Ok(())
    }

    #[test]
    fn fixture_allows_capability_component_without_driver() -> anyhow::Result<()> {
        let model = model_with_driver_block("")?;
        let structure = structure_with_fixture_mount()?;

        validate_model_components(
            &model,
            &structure,
            |_| drive_motor_component(),
            |_| false,
            true,
        )
    }

    #[test]
    fn component_with_driver_still_requires_driver_crate() -> anyhow::Result<()> {
        let model = model_with_driver_block(
            r#"
    driver:
      connection:
        type: usb
"#,
        )?;
        let structure = structure_with_fixture_mount()?;

        let error = validate_model_components(
            &model,
            &structure,
            |_| drive_motor_component(),
            |_| false,
            false,
        )
        .expect_err("driver-backed component without a crate should fail");

        assert!(
            error.to_string().contains(
                "component 'fixture_drive' provides driver config but component type 'drive_motor' has no components/drive_motor/Cargo.toml"
            ),
            "{error:#}"
        );
        Ok(())
    }

    fn model_with_driver_block(
        driver_block: &str,
    ) -> anyhow::Result<phoxal_utils_robot::v1::ModelV1> {
        let model = Model::read_from_string(&format!(
            r#"
version: v1

identity:
  model: fixture-validation

motion:
  kinematic:
    kind: differential
    left_actuators: [fixture_drive.motor]
    right_actuators: [fixture_drive.motor]
    left_encoders: [fixture_drive.encoder]
    right_encoders: [fixture_drive.encoder]
    wheel_radius_m: 0.10
    wheel_base_m: 0.40

  limits:
    max_linear_speed_mps: 0.6
    max_angular_speed_radps: 2.0
    max_linear_accel_mps2: 8.0
    max_linear_decel_mps2: 8.0
    max_angular_accel_radps2: 20.0

components:
  fixture_drive:
    component: drive_motor
    mount_link: fixture_mount{driver_block}
    parameters:
      motor:
        kind: motor
        direction_sign: 1
      encoder:
        kind: encoder
        direction_sign: 1
"#
        ))?;
        Ok(model.as_v1().expect("fixture model is v1").clone())
    }

    fn structure_with_fixture_mount() -> anyhow::Result<Structure> {
        Structure::from_urdf_str(
            r#"
<robot name="fixture_validation">
  <link name="base_footprint"/>
  <link name="base_link"/>
  <joint name="base_footprint_to_base_link" type="fixed">
    <parent link="base_footprint"/>
    <child link="base_link"/>
  </joint>
  <link name="fixture_mount"/>
  <joint name="base_link_to_fixture_mount" type="fixed">
    <parent link="base_link"/>
    <child link="fixture_mount"/>
  </joint>
</robot>
"#,
        )
    }

    fn drive_motor_component() -> anyhow::Result<phoxal_utils_component::Component> {
        phoxal_utils_component::Component::read_from_string(
            r#"
version: v1
capabilities:
  motor:
    kind: motor
    command: velocity
    target:
      kind: joint
      id: wheel_joint
  encoder:
    kind: encoder
    publish_rate_hz: 50.0
    target:
      kind: joint
      id: wheel_joint
"#,
        )
    }
}
