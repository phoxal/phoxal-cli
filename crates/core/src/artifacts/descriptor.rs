//! Converts resolved suite records into native artifact staging descriptors.

use anyhow::Result;

use crate::project::resolver::{
    ResolvedPlatformRuntime, ResolvedRobot, ResolvedTool, official_binary_name, tool_emit_apis_id,
};
use crate::project::suite::ArtifactKind;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProvisioningMode {
    MissingOnly,
}

#[derive(Debug, Clone)]
pub struct NativeArtifactDescriptor {
    pub package_id: String,
    pub kind: ArtifactKind,
    pub name: String,
    pub version: String,
    pub url: String,
    pub sha256: String,
    pub size: u64,
    pub binary_name: String,
    pub target: Option<String>,
}

impl NativeArtifactDescriptor {
    pub fn from_runtime(runtime: &ResolvedPlatformRuntime) -> Result<Option<Self>> {
        if runtime.path_override.is_some() {
            return Ok(None);
        }
        let binary_name = match runtime.kind {
            ArtifactKind::ComponentAssets => String::new(),
            _ => official_binary_name(runtime.kind, &runtime.name),
        };
        Ok(Some(Self {
            package_id: runtime.package.clone(),
            kind: runtime.kind,
            name: runtime.name.clone(),
            version: runtime.version.clone(),
            url: runtime.url.clone().unwrap_or_default(),
            sha256: runtime.sha256.clone().unwrap_or_default(),
            size: runtime.size.unwrap_or_default(),
            binary_name,
            target: runtime.target.clone(),
        }))
    }

    pub fn from_tool(tool: &ResolvedTool) -> Result<Option<Self>> {
        if !tool.published || tool.path_override.is_some() {
            return Ok(None);
        }
        Ok(Some(Self {
            package_id: tool.package.clone(),
            kind: tool.kind,
            name: tool_emit_apis_id(&tool.name).to_string(),
            version: tool.resolved.clone(),
            url: tool.url.clone().unwrap_or_default(),
            sha256: tool.sha256.clone(),
            size: tool.size.unwrap_or_default(),
            binary_name: tool.binary_name.clone(),
            target: Some(tool.target.clone()),
        }))
    }
}

pub fn descriptors(resolved: &ResolvedRobot) -> Result<Vec<NativeArtifactDescriptor>> {
    descriptors_for(resolved, true, true)
}

pub fn descriptors_for(
    resolved: &ResolvedRobot,
    include_simulators: bool,
    include_component_assets: bool,
) -> Result<Vec<NativeArtifactDescriptor>> {
    descriptors_for_drivers(
        resolved,
        include_simulators,
        include_component_assets,
        &crate::project::layout::DriverSelection::All,
    )
}

/// [`descriptors_for`] with an explicit driver selection: a component instance
/// excluded by the run's `DriverSelection` contributes no *driver* descriptor,
/// so `run`/`start`/`build` never require, check for, or download a driver the
/// policy excludes (#936, finding 2). Component *assets* are unaffected - a
/// driver-off instance still carries its component model - and the official,
/// simulator, and tool sets are unchanged.
pub fn descriptors_for_drivers(
    resolved: &ResolvedRobot,
    include_simulators: bool,
    include_component_assets: bool,
    drivers: &crate::project::layout::DriverSelection,
) -> Result<Vec<NativeArtifactDescriptor>> {
    let mut descriptors = Vec::new();
    for runtime in &resolved.platform_runtimes {
        if let Some(descriptor) = NativeArtifactDescriptor::from_runtime(runtime)? {
            descriptors.push(descriptor);
        }
    }
    if include_simulators {
        for runtime in &resolved.simulators {
            if let Some(descriptor) = NativeArtifactDescriptor::from_runtime(runtime)? {
                descriptors.push(descriptor);
            }
        }
    }
    for tool in &resolved.tools {
        if let Some(descriptor) = NativeArtifactDescriptor::from_tool(tool)? {
            descriptors.push(descriptor);
        }
    }
    let mut components = std::collections::BTreeSet::new();
    for component in &resolved.components {
        // The driver descriptor is dropped for a policy-excluded instance; its
        // assets descriptor (the component model) is always kept.
        let driver = drivers
            .includes_instance(&component.instance)
            .then_some(component.driver.as_ref())
            .flatten();
        let packages = driver.into_iter().chain(
            include_component_assets
                .then_some(component.assets.as_ref())
                .flatten(),
        );
        for package in packages {
            if let Some(runtime) = &package.suite_runtime
                && let Some(descriptor) = NativeArtifactDescriptor::from_runtime(runtime)?
                && components.insert((
                    descriptor.package_id.clone(),
                    descriptor.target.clone(),
                    descriptor.version.clone(),
                ))
            {
                descriptors.push(descriptor);
            }
        }
    }
    descriptors.sort_by(|left, right| {
        (&left.package_id, &left.target).cmp(&(&right.package_id, &right.target))
    });
    Ok(descriptors)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::layout::DriverSelection;
    use crate::project::resolver::{
        ResolvedComponent, ResolvedComponentPackage, ResolvedComponentSource, ResolvedRobot,
    };
    use std::collections::BTreeSet;

    fn driver_runtime(package: &str) -> ResolvedPlatformRuntime {
        ResolvedPlatformRuntime {
            name: package.rsplit('-').next().unwrap().to_string(),
            package: package.to_string(),
            kind: ArtifactKind::ComponentDriver,
            version: "0.1.0".to_string(),
            artifact_ref: "ref".to_string(),
            sha256: Some("a".repeat(64)),
            url: Some("https://example.com/driver.tar.gz".to_string()),
            size: Some(1),
            published: true,
            published_triples: vec!["host".to_string()],
            path_override: None,
            train: "0.1.0".to_string(),
            target: Some("host".to_string()),
        }
    }

    fn driven_component(instance: &str, package: &str) -> ResolvedComponent {
        ResolvedComponent {
            instance: instance.to_string(),
            source_name: package.rsplit('-').next().unwrap().to_string(),
            assets: None,
            driver: Some(ResolvedComponentPackage {
                package: package.to_string(),
                kind: ArtifactKind::ComponentDriver,
                source: ResolvedComponentSource::Suite,
                path_override: None,
                suite_runtime: Some(driver_runtime(package)),
            }),
            has_driver: true,
        }
    }

    fn resolved_with_two_drivers() -> Result<ResolvedRobot> {
        let yaml = r#"schema: robot/v0
robot:
  id: robot_v1
  namespace: dev
  motion_limits:
    max_linear_speed_mps: 0.6
    max_angular_speed_radps: 2.0
  structure: model/structure.urdf
  kinematic:
    kind: omnidirectional
    actuators: []
    encoders: []
  components: {}
"#;
        Ok(ResolvedRobot {
            robot: phoxal::model::robot::v0::Robot::parse_from_string(yaml)?,
            train: "0.1.0".to_string(),
            target: "host".to_string(),
            platform_runtimes: Vec::new(),
            simulators: Vec::new(),
            user_runtimes: Vec::new(),
            components: vec![
                driven_component("left_drive", "phoxal/component-left"),
                driven_component("right_drive", "phoxal/component-right"),
            ],
            tools: Vec::new(),
            path_overrides: Vec::new(),
        })
    }

    fn driver_packages(descriptors: &[NativeArtifactDescriptor]) -> BTreeSet<String> {
        descriptors
            .iter()
            .filter(|descriptor| descriptor.kind == ArtifactKind::ComponentDriver)
            .map(|descriptor| descriptor.package_id.clone())
            .collect()
    }

    /// `drivers off` (`DriverSelection::None`) drops every component-driver
    /// descriptor, so an excluded driver is never required or checked for
    /// vendored presence (#936, finding 2).
    #[test]
    fn drivers_off_excludes_all_driver_descriptors() -> Result<()> {
        let resolved = resolved_with_two_drivers()?;
        let descriptors = descriptors_for_drivers(&resolved, false, true, &DriverSelection::None)?;
        assert!(driver_packages(&descriptors).is_empty());
        Ok(())
    }

    /// A `--driver` subset keeps only the selected instances' driver
    /// descriptors; the excluded instance's driver is never required (#936,
    /// finding 2).
    #[test]
    fn driver_subset_keeps_only_selected_driver_descriptors() -> Result<()> {
        let resolved = resolved_with_two_drivers()?;
        let only_left = DriverSelection::Only(BTreeSet::from(["left_drive".to_string()]));
        let descriptors = descriptors_for_drivers(&resolved, false, true, &only_left)?;
        assert_eq!(
            driver_packages(&descriptors),
            BTreeSet::from(["phoxal/component-left".to_string()])
        );
        Ok(())
    }

    /// `drivers on` with no subset (`DriverSelection::All`) keeps every driver
    /// descriptor, matching the prior unconditional behavior.
    #[test]
    fn drivers_all_keeps_every_driver_descriptor() -> Result<()> {
        let resolved = resolved_with_two_drivers()?;
        let descriptors = descriptors_for_drivers(&resolved, false, true, &DriverSelection::All)?;
        assert_eq!(
            driver_packages(&descriptors),
            BTreeSet::from([
                "phoxal/component-left".to_string(),
                "phoxal/component-right".to_string(),
            ])
        );
        Ok(())
    }
}
