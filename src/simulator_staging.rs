use std::path::PathBuf;

use anyhow::Result;

use crate::host_paths;

pub(crate) fn cached_tool_path(name: &str, version: &str, binary_name: &str) -> Result<PathBuf> {
    Ok(host_paths::tools_cache_dir()?
        .join(name)
        .join(version)
        .join(binary_name))
}
