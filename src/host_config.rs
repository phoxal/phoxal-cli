use anyhow::{Context, Result};
use serde::Deserialize;

use crate::host_paths;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostConfig {
    #[serde(default)]
    pub zenoh_image: Option<String>,
}

pub fn load() -> Result<HostConfig> {
    let path = host_paths::config_path()?;
    if !path.exists() {
        return Ok(HostConfig::default());
    }
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read host config {}", path.display()))?;
    serde_yaml::from_str(&text)
        .with_context(|| format!("failed to parse host config {}", path.display()))
}
