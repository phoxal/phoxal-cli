//! Atomic file writes and validated artifact storage keys.

use anyhow::Context;
use anyhow::Result;
use phoxal_cli_core::artifacts::NativeArtifactDescriptor;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;

pub(crate) fn write_file_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .context("artifact store path did not have a parent directory")?;
    fs::create_dir_all(parent).with_context(|| format!("failed to create {}", parent.display()))?;
    let mut partial = tempfile::Builder::new()
        .prefix(".native-artifact-")
        .suffix(".partial")
        .tempfile_in(parent)
        .with_context(|| format!("failed to create temp artifact in {}", parent.display()))?;
    partial
        .write_all(bytes)
        .with_context(|| format!("failed to write {}", partial.path().display()))?;
    partial
        .persist(path)
        .map(|_| ())
        .map_err(|error| error.error)
        .with_context(|| format!("failed to finalize {}", path.display()))
}

pub(crate) fn package_storage_key(package: &str) -> Result<(String, String)> {
    let segments = package.split('/').collect::<Vec<_>>();
    anyhow::ensure!(
        segments.len() == 2,
        "artifact package must be provider-qualified as <provider>/<name>: {package:?}"
    );
    for segment in &segments {
        validate_path_segment("artifact package segment", segment)?;
    }
    Ok((segments[0].to_string(), segments[1].to_string()))
}

pub(crate) fn artifact_package_dir(package: &str) -> Result<PathBuf> {
    let (provider, package) = package_storage_key(package)?;
    Ok(crate::host_paths::artifacts_dir()?
        .join(provider)
        .join(package))
}

pub(crate) fn descriptor_scope_label(descriptor: &NativeArtifactDescriptor) -> &str {
    descriptor.target.as_deref().unwrap_or("assets")
}

pub(crate) fn validate_path_segment(label: &str, value: &str) -> Result<()> {
    anyhow::ensure!(
        !value.is_empty()
            && value != "."
            && value != ".."
            && value.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_' | b'+')
            }),
        "{label} contains unsafe path characters: {value:?}"
    );
    Ok(())
}
