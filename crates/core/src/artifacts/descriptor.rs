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
        let packages = component.driver.iter().chain(
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
