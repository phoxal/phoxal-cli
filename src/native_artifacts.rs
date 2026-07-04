use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use flate2::read::GzDecoder;
use sha2::{Digest, Sha256};
use tar::Archive;

use crate::catalog::ArtifactKind;
use crate::resolver::{
    ResolvedArtifactMetadata, ResolvedPlatformRuntime, ResolvedTool, official_binary_name,
};
use crate::ui::Ui;
use crate::utils::make_executable;

const FRAMEWORK_REPO: &str = "phoxal/framework";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProvisioningMode {
    MissingOnly,
    Refresh,
}

impl ProvisioningMode {
    #[must_use]
    pub const fn from_pull(pull: bool) -> Self {
        if pull {
            Self::Refresh
        } else {
            Self::MissingOnly
        }
    }
}

#[derive(Debug, Clone)]
pub struct NativeArtifactDescriptor {
    pub artifact_id: String,
    pub kind: ArtifactKind,
    pub name: String,
    pub version: String,
    pub asset: String,
    pub sha256: String,
    pub metadata: ResolvedArtifactMetadata,
    pub binary_name: String,
}

impl NativeArtifactDescriptor {
    pub fn from_runtime(runtime: &ResolvedPlatformRuntime) -> Result<Option<Self>> {
        let (Some(sha256), Some(metadata)) = (&runtime.sha256, &runtime.metadata) else {
            return Ok(None);
        };
        if runtime.path_override.is_some() {
            return Ok(None);
        }
        Ok(Some(Self {
            artifact_id: runtime.artifact_id.clone(),
            kind: runtime.kind,
            name: runtime.name.clone(),
            version: runtime.version.clone(),
            asset: runtime.artifact_ref().to_string(),
            sha256: sha256.clone(),
            metadata: metadata.clone(),
            binary_name: official_binary_name(runtime.kind, &runtime.name),
        }))
    }

    pub fn from_tool(tool: &ResolvedTool) -> Result<Option<Self>> {
        let Some(metadata) = &tool.metadata else {
            return Ok(None);
        };
        if tool.path_override.is_some() {
            return Ok(None);
        }
        Ok(Some(Self {
            artifact_id: tool.name.clone(),
            kind: ArtifactKind::Tool,
            name: tool
                .name
                .strip_prefix("tool-")
                .unwrap_or(&tool.name)
                .to_string(),
            version: tool.resolved.clone(),
            asset: tool.asset.clone(),
            sha256: tool.sha256.clone(),
            metadata: metadata.clone(),
            binary_name: tool.binary_name.clone(),
        }))
    }

    #[must_use]
    pub fn package(&self) -> String {
        format!("phoxal-{}-{}", self.kind, self.name)
    }

    #[must_use]
    pub fn tag(&self) -> String {
        format!("{}-v{}", self.package(), self.version)
    }
}

pub fn stage_runtime(
    ui: Option<&Ui>,
    runtime: &ResolvedPlatformRuntime,
    mode: ProvisioningMode,
) -> Result<Option<PathBuf>> {
    let Some(descriptor) = NativeArtifactDescriptor::from_runtime(runtime)? else {
        return Ok(None);
    };
    stage_descriptor(ui, &descriptor, mode).map(Some)
}

pub fn stage_tool(
    ui: Option<&Ui>,
    tool: &ResolvedTool,
    mode: ProvisioningMode,
) -> Result<Option<PathBuf>> {
    let Some(descriptor) = NativeArtifactDescriptor::from_tool(tool)? else {
        return Ok(None);
    };
    stage_descriptor(ui, &descriptor, mode).map(Some)
}

pub fn stage_resolved_artifacts(
    ui: Option<&Ui>,
    resolved: &crate::resolver::ResolvedRobot,
    mode: ProvisioningMode,
) -> Result<usize> {
    let mut count = 0;
    for runtime in &resolved.platform_runtimes {
        if stage_runtime(ui, runtime, mode)?.is_some() {
            count += 1;
        }
    }
    for simulator in &resolved.simulators {
        if stage_runtime(ui, simulator, mode)?.is_some() {
            count += 1;
        }
    }
    for tool in &resolved.tools {
        if stage_tool(ui, tool, mode)?.is_some() {
            count += 1;
        }
    }
    Ok(count)
}

pub fn stage_descriptor(
    ui: Option<&Ui>,
    descriptor: &NativeArtifactDescriptor,
    mode: ProvisioningMode,
) -> Result<PathBuf> {
    let root = artifact_cache_dir(descriptor)?;
    let metadata = metadata_path(descriptor)?;
    let binary = root.join(&descriptor.binary_name);
    if mode == ProvisioningMode::MissingOnly && metadata.is_file() {
        return Ok(binary);
    }

    if let Some(ui) = ui {
        ui.info(format!(
            "provisioning {} {} from {}",
            descriptor.kind,
            descriptor.name,
            descriptor.tag()
        ));
    }
    let asset_path = cached_asset_path(descriptor)?;
    if mode == ProvisioningMode::Refresh || !asset_path.is_file() {
        let bytes = download_release_asset(descriptor)?;
        write_file_atomic(&asset_path, &bytes)?;
    }
    verify_file_sha256(&asset_path, &descriptor.sha256)?;
    unpack_asset(&asset_path, &root)?;
    verify_file_sha256(&metadata, &descriptor.metadata.emit_apis_sha256)?;
    if binary.is_file() {
        make_executable(&binary)?;
    }
    Ok(binary)
}

pub fn read_packaged_emit_apis(descriptor: &NativeArtifactDescriptor) -> Result<String> {
    stage_descriptor(None, descriptor, ProvisioningMode::MissingOnly)?;
    let metadata = metadata_path(descriptor)?;
    fs::read_to_string(&metadata)
        .with_context(|| format!("failed to read packaged emit-apis {}", metadata.display()))
}

pub fn artifact_binary_path(descriptor: &NativeArtifactDescriptor) -> Result<PathBuf> {
    Ok(artifact_cache_dir(descriptor)?.join(&descriptor.binary_name))
}

pub fn artifact_cache_dir(descriptor: &NativeArtifactDescriptor) -> Result<PathBuf> {
    Ok(crate::host_paths::cache_dir()?
        .join("artifacts")
        .join(&descriptor.artifact_id)
        .join(sanitize_path_segment(&descriptor.asset)))
}

pub fn metadata_path(descriptor: &NativeArtifactDescriptor) -> Result<PathBuf> {
    Ok(artifact_cache_dir(descriptor)?.join(&descriptor.metadata.emit_apis))
}

fn cached_asset_path(descriptor: &NativeArtifactDescriptor) -> Result<PathBuf> {
    Ok(crate::host_paths::cache_dir()?
        .join("artifacts")
        .join("_assets")
        .join(&descriptor.artifact_id)
        .join(&descriptor.version)
        .join(&descriptor.asset))
}

fn release_asset_url(descriptor: &NativeArtifactDescriptor) -> String {
    format!(
        "https://github.com/{FRAMEWORK_REPO}/releases/download/{}/{}",
        descriptor.tag(),
        descriptor.asset
    )
}

fn download_release_asset(descriptor: &NativeArtifactDescriptor) -> Result<Vec<u8>> {
    let url = release_asset_url(descriptor);
    let client = reqwest::blocking::Client::builder()
        .user_agent("phoxal-cli")
        .timeout(Duration::from_secs(120))
        .build()?;
    let mut request = client.get(&url);
    if let Ok(token) = std::env::var("GITHUB_TOKEN") {
        request = request.bearer_auth(token);
    }
    let response = request
        .send()
        .with_context(|| format!("failed to download {url}"))?;
    if !response.status().is_success() {
        bail!(
            "download of {} from {} returned {}",
            descriptor.asset,
            descriptor.tag(),
            response.status()
        );
    }
    let bytes = response
        .bytes()
        .with_context(|| format!("failed to read {} body", descriptor.asset))?
        .to_vec();
    let actual = hex::encode(Sha256::digest(&bytes));
    if actual != descriptor.sha256 {
        bail!(
            "sha256 mismatch for {}: expected {}, got {actual}",
            descriptor.asset,
            descriptor.sha256
        );
    }
    Ok(bytes)
}

fn unpack_asset(asset_path: &Path, root: &Path) -> Result<()> {
    let partial = root.with_extension("partial");
    if partial.exists() {
        fs::remove_dir_all(&partial)
            .with_context(|| format!("failed to remove {}", partial.display()))?;
    }
    fs::create_dir_all(&partial)
        .with_context(|| format!("failed to create {}", partial.display()))?;

    if asset_path
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.ends_with(".tar.gz"))
    {
        unpack_tar_gz(asset_path, &partial)?;
    } else if asset_path
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.ends_with(".tar"))
    {
        unpack_tar(asset_path, &partial)?;
    } else {
        unpack_with_system_tar(asset_path, &partial)?;
    }

    if root.exists() {
        fs::remove_dir_all(root)
            .with_context(|| format!("failed to replace {}", root.display()))?;
    }
    fs::rename(&partial, root).with_context(|| format!("failed to finalize {}", root.display()))
}

fn unpack_tar_gz(asset_path: &Path, dest: &Path) -> Result<()> {
    let file = fs::File::open(asset_path)
        .with_context(|| format!("failed to open {}", asset_path.display()))?;
    let mut archive = Archive::new(GzDecoder::new(file));
    archive
        .unpack(dest)
        .with_context(|| format!("failed to unpack {}", asset_path.display()))
}

fn unpack_tar(asset_path: &Path, dest: &Path) -> Result<()> {
    let file = fs::File::open(asset_path)
        .with_context(|| format!("failed to open {}", asset_path.display()))?;
    let mut archive = Archive::new(file);
    archive
        .unpack(dest)
        .with_context(|| format!("failed to unpack {}", asset_path.display()))
}

fn unpack_with_system_tar(asset_path: &Path, dest: &Path) -> Result<()> {
    let status = Command::new("tar")
        .arg("-xf")
        .arg(asset_path)
        .arg("-C")
        .arg(dest)
        .status()
        .with_context(|| format!("failed to start tar for {}", asset_path.display()))?;
    if status.success() {
        Ok(())
    } else {
        Err(anyhow!(
            "tar failed with status {status} while unpacking {}",
            asset_path.display()
        ))
    }
}

fn verify_file_sha256(path: &Path, expected: &str) -> Result<()> {
    let actual = sha256_file(path)?;
    if actual != expected {
        bail!(
            "sha256 mismatch for {}: expected {expected}, got {actual}",
            path.display()
        );
    }
    Ok(())
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut file =
        fs::File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 8192];
    loop {
        let read = file
            .read(&mut buffer)
            .with_context(|| format!("failed to read {}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
}

fn write_file_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let partial = path.with_extension("partial");
    fs::write(&partial, bytes).with_context(|| format!("failed to write {}", partial.display()))?;
    fs::rename(&partial, path).with_context(|| format!("failed to finalize {}", path.display()))
}

fn sanitize_path_segment(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_') {
                ch
            } else {
                '_'
            }
        })
        .collect()
}
