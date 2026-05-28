use webots_proto::r2025a::{Accelerometer, Compass, GPS, Group, Gyro, InertialUnit, Node};
use webots_proto::types::ProtoField as WebotsField;

use crate::webots_staging::native_fields::{NativeValue, native_webots_fields_for_capability};
use crate::webots_staging::scene::WebotsSceneDescription;
use phoxal_component::v1::capability::Capability as PhysicalCapability;
use phoxal_simulation::capability::Capability as SimulationCapability;

impl WebotsSceneDescription {
    pub fn render_imu_family_capability(
        &self,
        capability_id: &str,
        physical: &PhysicalCapability,
        simulation: Option<&SimulationCapability>,
    ) -> Option<Node> {
        match (physical, simulation) {
            (PhysicalCapability::Imu(phys), Some(sim @ SimulationCapability::Imu(_))) => {
                let axes = phys.axes.unwrap_or([true, true, true]);
                let mut iu = InertialUnit::new(capability_id.to_string())
                    .with_x_axis(axes[0])
                    .with_y_axis(axes[1])
                    .with_z_axis(axes[2]);
                iu.name = WebotsField::Is(Self::capability_name_field_name(capability_id));

                let native_fields = native_webots_fields_for_capability(sim);
                for assignment in native_fields.assignments {
                    match assignment.field_name.as_str() {
                        "resolution" => {
                            if let NativeValue::Float(f) = assignment.value {
                                iu = iu.with_resolution(f);
                            }
                        }
                        "noise" => {
                            if let NativeValue::Float(f) = assignment.value {
                                iu = iu.with_noise(f);
                            }
                        }
                        _ => {}
                    }
                }

                let mut acc =
                    Accelerometer::new(Self::imu_accelerometer_capability_id(capability_id))
                        .with_x_axis(axes[0])
                        .with_y_axis(axes[1])
                        .with_z_axis(axes[2]);
                acc.name = WebotsField::Is(Self::imu_accelerometer_name_field_name(capability_id));

                let mut gyro = Gyro::new(Self::imu_gyroscope_capability_id(capability_id))
                    .with_x_axis(axes[0])
                    .with_y_axis(axes[1])
                    .with_z_axis(axes[2]);
                gyro.name = WebotsField::Is(Self::imu_gyroscope_name_field_name(capability_id));

                let native_fields = native_webots_fields_for_capability(sim);
                for assignment in native_fields.assignments {
                    if assignment.field_name == "resolution"
                        && let NativeValue::Float(f) = assignment.value
                    {
                        acc = acc.with_resolution(f);
                        gyro = gyro.with_resolution(f);
                    }
                }

                Some(Node::Group(Group::new().with_children(vec![
                    Node::InertialUnit(iu),
                    Node::Accelerometer(acc),
                    Node::Gyro(gyro),
                ])))
            }
            (
                PhysicalCapability::Accelerometer(phys),
                Some(sim @ SimulationCapability::Accelerometer(_)),
            ) => {
                let axes = phys.axes.unwrap_or([true, true, true]);
                let mut acc = Accelerometer::new(capability_id.to_string())
                    .with_x_axis(axes[0])
                    .with_y_axis(axes[1])
                    .with_z_axis(axes[2]);
                acc.name = WebotsField::Is(Self::capability_name_field_name(capability_id));

                let native_fields = native_webots_fields_for_capability(sim);
                for assignment in native_fields.assignments {
                    match assignment.field_name.as_str() {
                        "resolution" => {
                            if let NativeValue::Float(f) = assignment.value {
                                acc = acc.with_resolution(f);
                            }
                        }
                        "lookupTable" => {
                            if let NativeValue::LookupTable(lt) = assignment.value {
                                acc = acc.with_lookup_table(Self::lookup_table_from_native(&lt));
                            }
                        }
                        _ => {}
                    }
                }
                Some(Node::Accelerometer(acc))
            }
            (
                PhysicalCapability::Gyroscope(phys),
                Some(sim @ SimulationCapability::Gyroscope(_)),
            ) => {
                let axes = phys.axes.unwrap_or([true, true, true]);
                let mut gyro = Gyro::new(capability_id.to_string())
                    .with_x_axis(axes[0])
                    .with_y_axis(axes[1])
                    .with_z_axis(axes[2]);
                gyro.name = WebotsField::Is(Self::capability_name_field_name(capability_id));

                let native_fields = native_webots_fields_for_capability(sim);
                for assignment in native_fields.assignments {
                    match assignment.field_name.as_str() {
                        "resolution" => {
                            if let NativeValue::Float(f) = assignment.value {
                                gyro = gyro.with_resolution(f);
                            }
                        }
                        "lookupTable" => {
                            if let NativeValue::LookupTable(lt) = assignment.value {
                                gyro = gyro.with_lookup_table(Self::lookup_table_from_native(&lt));
                            }
                        }
                        _ => {}
                    }
                }
                Some(Node::Gyro(gyro))
            }
            (
                PhysicalCapability::Magnetometer(_),
                Some(sim @ SimulationCapability::Magnetometer(_)),
            ) => {
                let mut compass = Compass::new(capability_id.to_string());
                compass.name = WebotsField::Is(Self::capability_name_field_name(capability_id));
                let native_fields = native_webots_fields_for_capability(sim);
                for assignment in native_fields.assignments {
                    match assignment.field_name.as_str() {
                        "resolution" => {
                            if let NativeValue::Float(f) = assignment.value {
                                compass = compass.with_resolution(f);
                            }
                        }
                        "lookupTable" => {
                            if let NativeValue::LookupTable(lt) = assignment.value {
                                compass =
                                    compass.with_lookup_table(Self::lookup_table_from_native(&lt));
                            }
                        }
                        _ => {}
                    }
                }
                Some(Node::Compass(compass))
            }
            (PhysicalCapability::Gnss(_), Some(sim @ SimulationCapability::Gnss(_))) => {
                let mut gps = GPS::new(capability_id.to_string());
                gps.name = WebotsField::Is(Self::capability_name_field_name(capability_id));
                let native_fields = native_webots_fields_for_capability(sim);
                for assignment in native_fields.assignments {
                    match assignment.field_name.as_str() {
                        "accuracy" => {
                            if let NativeValue::Float(f) = assignment.value {
                                gps = gps.with_accuracy(f);
                            }
                        }
                        "noiseCorrelation" => {
                            if let NativeValue::Float(f) = assignment.value {
                                gps = gps.with_noise_correlation(f);
                            }
                        }
                        "resolution" => {
                            if let NativeValue::Float(f) = assignment.value {
                                gps = gps.with_resolution(f);
                            }
                        }
                        "speedResolution" => {
                            if let NativeValue::Float(f) = assignment.value {
                                gps = gps.with_speed_resolution(f);
                            }
                        }
                        "speedNoise" => {
                            if let NativeValue::Float(f) = assignment.value {
                                gps = gps.with_speed_noise(f);
                            }
                        }
                        _ => {}
                    }
                }
                Some(Node::GPS(gps))
            }
            _ => None,
        }
    }
}
