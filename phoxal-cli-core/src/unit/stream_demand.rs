use std::collections::BTreeMap;

use anyhow::{Result, bail};
use phoxal_component_api::v1::{
    CameraStreamDemand, DepthStreamDemand, RuntimeStreamDemand,
    capability::profile::{CameraProfileEncoding, DepthProfileEncoding},
};
use phoxal_runtime_localize_api::v1::LocalizeStreamDemands;
use phoxal_utils_component::v1::CapabilityRef;
use phoxal_utils_component::v1::capability::{Camera, Capability, Depth};
use phoxal_utils_robot::Robot;
use phoxal_utils_robot::v1::{LocalizeBackendKind, ResolvedCapabilityRole};

pub fn validate_runtime_stream_demands(
    model: &Robot,
    components_by_type: &BTreeMap<String, phoxal_utils_component::v1::Component>,
    framework_runtimes: &[&str],
    localize_backend: LocalizeBackendKind,
    roles: &[ResolvedCapabilityRole],
) -> Result<Vec<String>> {
    let components_by_instance = components_by_instance(model, components_by_type)?;
    let mut warnings = Vec::new();
    let mut errors = Vec::new();

    for &runtime_name in framework_runtimes {
        for demand in runtime_stream_demands(runtime_name, localize_backend) {
            match validate_runtime_demand(runtime_name, &demand, roles, &components_by_instance) {
                Ok(Some(warning)) => warnings.push(warning),
                Ok(None) => {}
                Err(error) => errors.push(error),
            }
        }
    }

    if errors.is_empty() {
        Ok(warnings)
    } else {
        bail!("Runtime stream demand errors:\n{}", errors.join("\n"))
    }
}

fn runtime_stream_demands(
    runtime_name: &str,
    localize_backend: LocalizeBackendKind,
) -> Vec<RuntimeStreamDemand> {
    match runtime_name {
        "localize" => LocalizeStreamDemands::for_backend(localize_backend),
        _ => Vec::new(),
    }
}

fn components_by_instance(
    model: &Robot,
    components_by_type: &BTreeMap<String, phoxal_utils_component::v1::Component>,
) -> Result<BTreeMap<CapabilityRef, Capability>> {
    let mut capabilities = BTreeMap::new();

    for (component_id, component_instance) in &model.components {
        let Some(component) = components_by_type.get(&component_instance.component) else {
            bail!(
                "component instance '{}' uses unstaged component type '{}'",
                component_id,
                component_instance.component
            );
        };
        for (capability_id, capability) in &component.capabilities {
            capabilities.insert(
                CapabilityRef::new(component_id, capability_id),
                capability.clone(),
            );
        }
    }

    Ok(capabilities)
}

fn validate_runtime_demand(
    runtime_name: &str,
    demand: &RuntimeStreamDemand,
    roles: &[ResolvedCapabilityRole],
    capabilities: &BTreeMap<CapabilityRef, Capability>,
) -> std::result::Result<Option<String>, String> {
    let matches = matching_capabilities(demand, roles, capabilities);
    if matches.is_empty() {
        return Err(format!(
            "runtime '{}' {} has no assigned {} capability; assigned role capabilities: {}",
            runtime_name,
            demand_label(demand),
            demand_kind(demand),
            role_offers(demand_role(demand), roles, capabilities)
        ));
    }

    let mut any_satisfying_native = false;
    let mut offers = Vec::new();

    for (capability_ref, capability) in matches {
        match (demand, capability) {
            (RuntimeStreamDemand::Camera(camera_demand), Capability::Camera(camera)) => {
                any_satisfying_native |= camera_native_satisfies(camera_demand, camera);
                offers.push(format!(
                    "{} [{}]",
                    capability_ref,
                    format_camera_native(camera)
                ));
            }
            (RuntimeStreamDemand::Depth(depth_demand), Capability::Depth(depth)) => {
                any_satisfying_native |= depth_native_satisfies(depth_demand, depth);
                offers.push(format!(
                    "{} [{}]",
                    capability_ref,
                    format_depth_native(depth)
                ));
            }
            _ => {}
        }
    }

    if !any_satisfying_native {
        return Err(format!(
            "runtime '{}' {} cannot be satisfied by native capability envelope; requires {}; native envelopes: {}",
            runtime_name,
            demand_label(demand),
            demand_floor(demand),
            offers.join("; ")
        ));
    }

    Ok(None)
}

fn matching_capabilities<'a>(
    demand: &RuntimeStreamDemand,
    roles: &'a [ResolvedCapabilityRole],
    capabilities: &'a BTreeMap<CapabilityRef, Capability>,
) -> Vec<(&'a CapabilityRef, &'a Capability)> {
    roles
        .iter()
        .filter(|assignment| {
            assignment
                .roles
                .iter()
                .any(|role| role.as_str() == demand_role(demand))
        })
        .filter_map(|assignment| {
            capabilities
                .get(&assignment.capability)
                .filter(|capability| capability_matches_demand_kind(demand, capability))
                .map(|capability| (&assignment.capability, capability))
        })
        .collect()
}

fn capability_matches_demand_kind(demand: &RuntimeStreamDemand, capability: &Capability) -> bool {
    matches!(
        (demand, capability),
        (RuntimeStreamDemand::Camera(_), Capability::Camera(_))
            | (RuntimeStreamDemand::Depth(_), Capability::Depth(_))
    )
}

fn camera_native_satisfies(demand: &CameraStreamDemand, camera: &Camera) -> bool {
    camera.publish_rate_hz >= demand.min_rate_hz
        && camera.width_px >= demand.min_width_px
        && camera.height_px >= demand.min_height_px
        && demand.accepted_encodings.iter().any(|encoding| {
            matches!(
                encoding,
                CameraProfileEncoding::Rgb8 | CameraProfileEncoding::Rgba8
            )
        })
}

fn depth_native_satisfies(demand: &DepthStreamDemand, depth: &Depth) -> bool {
    depth.publish_rate_hz >= demand.min_rate_hz
        && depth.width_px >= demand.min_width_px
        && depth.height_px >= demand.min_height_px
        && demand
            .accepted_encodings
            .contains(&DepthProfileEncoding::U16Millimeters)
}

fn demand_role(demand: &RuntimeStreamDemand) -> &'static str {
    match demand {
        RuntimeStreamDemand::Camera(demand) => demand.role,
        RuntimeStreamDemand::Depth(demand) => demand.role,
    }
}

fn demand_kind(demand: &RuntimeStreamDemand) -> &'static str {
    match demand {
        RuntimeStreamDemand::Camera(_) => "camera",
        RuntimeStreamDemand::Depth(_) => "depth",
    }
}

fn demand_label(demand: &RuntimeStreamDemand) -> String {
    format!("{} {} demand", demand_role(demand), demand_kind(demand))
}

fn demand_floor(demand: &RuntimeStreamDemand) -> String {
    match demand {
        RuntimeStreamDemand::Camera(demand) => format!(
            "rate >= {:.1} Hz, resolution >= {}x{}, encoding in [{}]",
            demand.min_rate_hz,
            demand.min_width_px,
            demand.min_height_px,
            demand
                .accepted_encodings
                .iter()
                .map(camera_encoding_name)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        RuntimeStreamDemand::Depth(demand) => format!(
            "rate >= {:.1} Hz, resolution >= {}x{}, encoding in [{}]",
            demand.min_rate_hz,
            demand.min_width_px,
            demand.min_height_px,
            demand
                .accepted_encodings
                .iter()
                .map(depth_encoding_name)
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

fn role_offers(
    role: &str,
    roles: &[ResolvedCapabilityRole],
    capabilities: &BTreeMap<CapabilityRef, Capability>,
) -> String {
    let offers = roles
        .iter()
        .filter(|assignment| {
            assignment
                .roles
                .iter()
                .any(|assigned_role| assigned_role.as_str() == role)
        })
        .map(
            |assignment| match capabilities.get(&assignment.capability) {
                Some(capability) => {
                    format!("{} ({})", assignment.capability, capability.kind_name())
                }
                None => format!("{} (missing)", assignment.capability),
            },
        )
        .collect::<Vec<_>>();

    if offers.is_empty() {
        "<none>".to_string()
    } else {
        offers.join(", ")
    }
}

fn format_camera_native(camera: &Camera) -> String {
    format!(
        "native/default: {:.1} Hz {}x{} {}",
        camera.publish_rate_hz,
        camera.width_px,
        camera.height_px,
        camera_encoding_name(&CameraProfileEncoding::Rgba8)
    )
}

fn format_depth_native(depth: &Depth) -> String {
    format!(
        "native/default: {:.1} Hz {}x{} {}",
        depth.publish_rate_hz,
        depth.width_px,
        depth.height_px,
        depth_encoding_name(&DepthProfileEncoding::U16Millimeters)
    )
}

fn camera_encoding_name(encoding: &CameraProfileEncoding) -> &'static str {
    match encoding {
        CameraProfileEncoding::L8 => "l8",
        CameraProfileEncoding::Rgb8 => "rgb8",
        CameraProfileEncoding::Rgba8 => "rgba8",
        CameraProfileEncoding::Jpeg => "jpeg",
        CameraProfileEncoding::Png => "png",
    }
}

fn depth_encoding_name(encoding: &DepthProfileEncoding) -> &'static str {
    match encoding {
        DepthProfileEncoding::U16Millimeters => "u16_millimeters",
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use phoxal_utils_component::v1::capability::{
        Camera, CameraMode, Capability, Depth, Imu, StructuralTarget,
    };
    use phoxal_utils_robot::v1::{
        Component, KinematicConfig, Motion, ResolvedCapabilityRole, Role,
    };
    use phoxal_utils_robot::{
        ComponentSource, Components, Identity, Phoxal, PhoxalRuntimes, Sim, SourcePath, Version,
    };

    use super::*;

    #[test]
    fn validates_demands_satisfied_by_native_envelopes() {
        let (model, components) = fixture_robot_source(15.0);
        let roles = resolved_roles();

        let warnings = validate_runtime_stream_demands(
            &model,
            &components,
            &["localize"],
            LocalizeBackendKind::OrbSlam3RgbdInertial,
            &roles,
        )
        .unwrap();

        assert!(warnings.is_empty());
    }

    #[test]
    fn rejects_camera_native_below_orb_demand_floor() {
        let (model, components) = fixture_robot_source(1.0);
        let roles = resolved_roles();

        let error = validate_runtime_stream_demands(
            &model,
            &components,
            &["localize"],
            LocalizeBackendKind::OrbSlam3RgbdInertial,
            &roles,
        )
        .unwrap_err();
        let message = format!("{error:#}");

        assert!(message.contains("runtime 'localize' localization camera demand"));
        assert!(message.contains("cannot be satisfied by native capability envelope"));
        assert!(message.contains("native/default: 1.0 Hz 640x480 rgba8"));
    }

    #[test]
    fn rejects_depth_native_below_orb_demand_floor() {
        let (model, components) = fixture_robot_source_with_rates(15.0, 1.0);
        let roles = resolved_roles();

        let error = validate_runtime_stream_demands(
            &model,
            &components,
            &["localize"],
            LocalizeBackendKind::OrbSlam3RgbdInertial,
            &roles,
        )
        .unwrap_err();
        let message = format!("{error:#}");

        assert!(message.contains("runtime 'localize' localization depth demand"));
        assert!(message.contains("cannot be satisfied by native capability envelope"));
        assert!(message.contains("native/default: 1.0 Hz 640x480 u16_millimeters"));
    }

    fn resolved_roles() -> Vec<ResolvedCapabilityRole> {
        vec![
            ResolvedCapabilityRole {
                capability: CapabilityRef::new("front_camera", "rgb"),
                roles: vec![Role::Localization],
            },
            ResolvedCapabilityRole {
                capability: CapabilityRef::new("front_camera", "depth"),
                roles: vec![Role::Localization],
            },
            ResolvedCapabilityRole {
                capability: CapabilityRef::new("imu", "imu"),
                roles: vec![Role::Localization],
            },
        ]
    }

    fn fixture_robot_source(
        camera_rate_hz: f64,
    ) -> (
        Robot,
        BTreeMap<String, phoxal_utils_component::v1::Component>,
    ) {
        fixture_robot_source_with_rates(camera_rate_hz, 15.0)
    }

    fn fixture_robot_source_with_rates(
        camera_rate_hz: f64,
        depth_rate_hz: f64,
    ) -> (
        Robot,
        BTreeMap<String, phoxal_utils_component::v1::Component>,
    ) {
        let model = Robot {
            version: Version::V1,
            phoxal: Phoxal {
                cli_min_version: "^0.6".to_string(),
            },
            identity: Identity {
                id: "stream-demand-fixture".to_string(),
                namespace: "dev".to_string(),
            },
            structure: "structure.urdf".into(),
            phoxal_runtimes: PhoxalRuntimes {
                version: "^0.1".to_string(),
                overrides: BTreeMap::new(),
            },
            user_runtimes: BTreeMap::new(),
            sim: Sim {
                world: "sim/worlds/test.wbt".into(),
            },
            tools: BTreeMap::new(),
            motion: Motion {
                kinematic: KinematicConfig::Differential {
                    left_actuators: vec![CapabilityRef::new("left_motor", "motor")],
                    right_actuators: vec![CapabilityRef::new("right_motor", "motor")],
                    left_encoders: vec![],
                    right_encoders: vec![],
                    wheel_radius_m: 0.08,
                    wheel_base_m: 0.4,
                },
            },
            components: Components {
                sources: BTreeMap::from([
                    (
                        "camera_rgbd".to_string(),
                        ComponentSource::Path(SourcePath {
                            path: "./components/camera_rgbd".into(),
                        }),
                    ),
                    (
                        "imu".to_string(),
                        ComponentSource::Path(SourcePath {
                            path: "./components/imu".into(),
                        }),
                    ),
                ]),
                instances: BTreeMap::from([
                    (
                        "front_camera".to_string(),
                        Component {
                            component: "camera_rgbd".to_string(),
                            mount_link: "base_link".to_string(),
                            roles: BTreeMap::from([
                                ("rgb".to_string(), vec![Role::Localization]),
                                ("depth".to_string(), vec![Role::Localization]),
                            ]),
                            parameters: BTreeMap::new(),
                            driver: None,
                        },
                    ),
                    (
                        "imu".to_string(),
                        Component {
                            component: "imu".to_string(),
                            mount_link: "base_link".to_string(),
                            roles: BTreeMap::from([("imu".to_string(), vec![Role::Localization])]),
                            parameters: BTreeMap::new(),
                            driver: None,
                        },
                    ),
                ]),
            },
        };
        let components = BTreeMap::from([
            (
                "camera_rgbd".to_string(),
                phoxal_utils_component::v1::Component {
                    capabilities: BTreeMap::from([
                        (
                            "rgb".to_string(),
                            Capability::Camera(Camera {
                                target: link_target(),
                                mode: CameraMode::Rgb,
                                publish_rate_hz: camera_rate_hz,
                                width_px: 640,
                                height_px: 480,
                                field_of_view_rad: Some(1.2),
                            }),
                        ),
                        (
                            "depth".to_string(),
                            Capability::Depth(Depth {
                                target: link_target(),
                                publish_rate_hz: depth_rate_hz,
                                width_px: 640,
                                height_px: 480,
                                field_of_view_rad: Some(1.2),
                                min_range_m: None,
                                max_range_m: None,
                            }),
                        ),
                    ]),
                },
            ),
            (
                "imu".to_string(),
                phoxal_utils_component::v1::Component {
                    capabilities: BTreeMap::from([(
                        "imu".to_string(),
                        Capability::Imu(Imu {
                            target: link_target(),
                            publish_rate_hz: 100.0,
                            axes: None,
                        }),
                    )]),
                },
            ),
        ]);

        (model, components)
    }

    fn link_target() -> StructuralTarget {
        StructuralTarget::Link {
            id: "camera_link".to_string(),
        }
    }
}
