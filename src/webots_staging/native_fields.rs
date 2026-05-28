//! Native Webots field mapping.
//!
//! Only values that map to real Webots node fields belong here.

use anyhow::{Result, anyhow};
use phoxal_simulation::capability::Capability as SimulationCapability;

/// A semantic value for a native Webots field.
#[derive(Debug, Clone, PartialEq)]
pub enum NativeValue {
    Float(f64),
    Bool(bool),
    String(String), // Raw semantic string (unquoted)
    Vec3([f64; 3]),
    LookupTable(Vec<[f64; 3]>),
}

/// A single field that belongs in the generated PROTO body.
#[derive(Debug, Clone)]
pub struct NativeWebotsFieldAssignment {
    pub field_name: String,
    pub value: NativeValue,
}

/// All native Webots fields for a single capability (one device).
#[derive(Debug, Clone, Default)]
pub struct NativeWebotsFields {
    pub assignments: Vec<NativeWebotsFieldAssignment>,
}

// ───────────────────────────────────────────────────────────────────────────
// Public contract helpers
// ───────────────────────────────────────────────────────────────────────────

/// Return the native Webots fields that must appear in the PROTO body.
pub fn native_webots_fields_for_capability(
    capability: &SimulationCapability,
) -> NativeWebotsFields {
    match capability {
        SimulationCapability::Encoder(cfg) => {
            let mut fields = NativeWebotsFields::default();
            if let Some(res) = cfg.resolution {
                fields.assignments.push(NativeWebotsFieldAssignment {
                    field_name: "resolution".to_string(),
                    value: NativeValue::Float(res),
                });
            }
            if let Some(noise) = cfg.noise {
                fields.assignments.push(NativeWebotsFieldAssignment {
                    field_name: "noise".to_string(),
                    value: NativeValue::Float(noise),
                });
            }
            fields
        }
        SimulationCapability::Accelerometer(cfg) => {
            let mut fields = NativeWebotsFields::default();
            if let Some(res) = cfg.resolution {
                fields.assignments.push(NativeWebotsFieldAssignment {
                    field_name: "resolution".to_string(),
                    value: NativeValue::Float(res),
                });
            }
            if let Some(lookup_table) = &cfg.lookup_table {
                fields.assignments.push(NativeWebotsFieldAssignment {
                    field_name: "lookupTable".to_string(),
                    value: NativeValue::LookupTable(
                        lookup_table
                            .iter()
                            .filter(|entry| entry.len() == 3)
                            .map(|entry| [entry[0], entry[1], entry[2]])
                            .collect(),
                    ),
                });
            }
            fields
        }
        SimulationCapability::Gyroscope(cfg) => {
            let mut fields = NativeWebotsFields::default();
            if let Some(res) = cfg.resolution {
                fields.assignments.push(NativeWebotsFieldAssignment {
                    field_name: "resolution".to_string(),
                    value: NativeValue::Float(res),
                });
            }
            if let Some(lookup_table) = &cfg.lookup_table {
                fields.assignments.push(NativeWebotsFieldAssignment {
                    field_name: "lookupTable".to_string(),
                    value: NativeValue::LookupTable(
                        lookup_table
                            .iter()
                            .filter(|entry| entry.len() == 3)
                            .map(|entry| [entry[0], entry[1], entry[2]])
                            .collect(),
                    ),
                });
            }
            fields
        }
        SimulationCapability::Magnetometer(cfg) => {
            let mut fields = NativeWebotsFields::default();
            if let Some(res) = cfg.resolution {
                fields.assignments.push(NativeWebotsFieldAssignment {
                    field_name: "resolution".to_string(),
                    value: NativeValue::Float(res),
                });
            }
            if let Some(lookup_table) = &cfg.lookup_table {
                fields.assignments.push(NativeWebotsFieldAssignment {
                    field_name: "lookupTable".to_string(),
                    value: NativeValue::LookupTable(
                        lookup_table
                            .iter()
                            .filter(|entry| entry.len() == 3)
                            .map(|entry| [entry[0], entry[1], entry[2]])
                            .collect(),
                    ),
                });
            }
            fields
        }
        SimulationCapability::Imu(cfg) => {
            let mut fields = NativeWebotsFields::default();
            if let Some(res) = cfg.resolution {
                fields.assignments.push(NativeWebotsFieldAssignment {
                    field_name: "resolution".to_string(),
                    value: NativeValue::Float(res),
                });
            }
            if let Some(noise) = cfg.noise {
                fields.assignments.push(NativeWebotsFieldAssignment {
                    field_name: "noise".to_string(),
                    value: NativeValue::Float(noise),
                });
            }
            fields
        }
        SimulationCapability::Gnss(cfg) => {
            let mut fields = NativeWebotsFields::default();
            if let Some(accuracy) = cfg.accuracy {
                fields.assignments.push(NativeWebotsFieldAssignment {
                    field_name: "accuracy".to_string(),
                    value: NativeValue::Float(accuracy),
                });
            }
            if let Some(noise_correlation) = cfg.noise_correlation {
                fields.assignments.push(NativeWebotsFieldAssignment {
                    field_name: "noiseCorrelation".to_string(),
                    value: NativeValue::Float(noise_correlation),
                });
            }
            if let Some(res) = cfg.resolution {
                fields.assignments.push(NativeWebotsFieldAssignment {
                    field_name: "resolution".to_string(),
                    value: NativeValue::Float(res),
                });
            }
            if let Some(res) = cfg.speed_resolution {
                fields.assignments.push(NativeWebotsFieldAssignment {
                    field_name: "speedResolution".to_string(),
                    value: NativeValue::Float(res),
                });
            }
            if let Some(noise) = cfg.speed_noise {
                fields.assignments.push(NativeWebotsFieldAssignment {
                    field_name: "speedNoise".to_string(),
                    value: NativeValue::Float(noise),
                });
            }
            fields
        }
        SimulationCapability::Camera(cfg) => {
            let mut fields = NativeWebotsFields::default();
            if let Some(projection) = cfg.projection {
                fields.assignments.push(NativeWebotsFieldAssignment {
                    field_name: "projection".to_string(),
                    value: NativeValue::String(
                        match projection {
                            phoxal_simulation::capability::CameraProjection::Planar => "planar",
                            phoxal_simulation::capability::CameraProjection::Cylindrical => {
                                "cylindrical"
                            }
                            phoxal_simulation::capability::CameraProjection::Spherical => {
                                "spherical"
                            }
                        }
                        .to_string(),
                    ),
                });
            }
            if let Some(near) = cfg.near {
                fields.assignments.push(NativeWebotsFieldAssignment {
                    field_name: "near".to_string(),
                    value: NativeValue::Float(near),
                });
            }
            if let Some(far) = cfg.far {
                fields.assignments.push(NativeWebotsFieldAssignment {
                    field_name: "far".to_string(),
                    value: NativeValue::Float(far),
                });
            }
            if let Some(exposure) = cfg.exposure {
                fields.assignments.push(NativeWebotsFieldAssignment {
                    field_name: "exposure".to_string(),
                    value: NativeValue::Float(exposure),
                });
            }
            if let Some(anti_aliasing) = cfg.anti_aliasing {
                fields.assignments.push(NativeWebotsFieldAssignment {
                    field_name: "antiAliasing".to_string(),
                    value: NativeValue::Bool(anti_aliasing),
                });
            }
            if let Some(radius) = cfg.ambient_occlusion_radius {
                fields.assignments.push(NativeWebotsFieldAssignment {
                    field_name: "ambientOcclusionRadius".to_string(),
                    value: NativeValue::Float(radius),
                });
            }
            if let Some(threshold) = cfg.bloom_threshold {
                fields.assignments.push(NativeWebotsFieldAssignment {
                    field_name: "bloomThreshold".to_string(),
                    value: NativeValue::Float(threshold),
                });
            }
            if let Some(noise) = cfg.noise {
                fields.assignments.push(NativeWebotsFieldAssignment {
                    field_name: "noise".to_string(),
                    value: NativeValue::Float(noise),
                });
            }
            if let Some(blur) = cfg.motion_blur {
                fields.assignments.push(NativeWebotsFieldAssignment {
                    field_name: "motionBlur".to_string(),
                    value: NativeValue::Float(blur),
                });
            }
            if let Some(noise_mask_url) = &cfg.noise_mask_url {
                fields.assignments.push(NativeWebotsFieldAssignment {
                    field_name: "noiseMaskUrl".to_string(),
                    value: NativeValue::String(noise_mask_url.clone()),
                });
            }
            fields
        }
        SimulationCapability::Depth(cfg) => {
            let mut fields = NativeWebotsFields::default();
            // Reconciled with R2025a RangeFinder support for depth-camera output.
            if let Some(res) = cfg.resolution {
                fields.assignments.push(NativeWebotsFieldAssignment {
                    field_name: "resolution".to_string(),
                    value: NativeValue::Float(res),
                });
            }
            if let Some(noise) = cfg.noise {
                fields.assignments.push(NativeWebotsFieldAssignment {
                    field_name: "noise".to_string(),
                    value: NativeValue::Float(noise),
                });
            }
            if let Some(blur) = cfg.motion_blur {
                fields.assignments.push(NativeWebotsFieldAssignment {
                    field_name: "motionBlur".to_string(),
                    value: NativeValue::Float(blur),
                });
            }
            fields
        }
        SimulationCapability::Lidar(cfg) => {
            let mut fields = NativeWebotsFields::default();
            if let Some(noise) = cfg.noise {
                fields.assignments.push(NativeWebotsFieldAssignment {
                    field_name: "noise".to_string(),
                    value: NativeValue::Float(noise),
                });
            }
            if let Some(res) = cfg.resolution {
                fields.assignments.push(NativeWebotsFieldAssignment {
                    field_name: "resolution".to_string(),
                    value: NativeValue::Float(res),
                });
            }
            fields
        }
        SimulationCapability::Microphone(cfg) => {
            let mut fields = NativeWebotsFields::default();
            if let Some(aperture) = cfg.aperture {
                fields.assignments.push(NativeWebotsFieldAssignment {
                    field_name: "aperture".to_string(),
                    value: NativeValue::Float(aperture),
                });
            }
            fields
        }
        // Mmwave removed for now as it is not currently applied in renderer (Finding 6).
        SimulationCapability::Mmwave(_)
        | SimulationCapability::Range(_)
        | SimulationCapability::Motor(_)
        | SimulationCapability::Battery
        | SimulationCapability::Led
        | SimulationCapability::Speaker => NativeWebotsFields::default(),
    }
}

/// Emits the native Webots motor-specific fields (acceleration, controlPID).
pub fn native_webots_motor_fields(
    cfg: &phoxal_simulation::capability::Motor,
) -> Result<NativeWebotsFields> {
    let mut fields = NativeWebotsFields::default();
    if let Some(acc) = cfg.acceleration_radps2 {
        fields.assignments.push(NativeWebotsFieldAssignment {
            field_name: "acceleration".to_string(),
            value: NativeValue::Float(acc),
        });
    }
    if let Some(pid) = &cfg.control_pid {
        if pid.len() > 3 {
            return Err(anyhow!(
                "control_pid must contain at most 3 values; found {}",
                pid.len()
            ));
        }
        let mut vals = [0.0; 3];
        for (i, val) in pid.iter().enumerate() {
            vals[i] = *val;
        }
        fields.assignments.push(NativeWebotsFieldAssignment {
            field_name: "controlPID".to_string(),
            value: NativeValue::Vec3(vals),
        });
    }
    Ok(fields)
}

/// Return the native Webots `contactMaterial` field for a Solid node.
pub fn native_webots_contact_material_for_link(
    contact_material_name: Option<&str>,
) -> Option<NativeWebotsFieldAssignment> {
    contact_material_name.map(|name| NativeWebotsFieldAssignment {
        field_name: "contactMaterial".to_string(),
        value: NativeValue::String(name.to_string()),
    })
}

// ───────────────────────────────────────────────────────────────────────────
// Tests
// ───────────────────────────────────────────────────────────────────────────
