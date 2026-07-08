use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use flate2::read::GzDecoder;
use sha2::{Digest, Sha256};
use tar::Archive;

use crate::catalog::{ArtifactKind, LocalManifestUpsert};
use crate::resolver::{ResolvedPlatformRuntime, ResolvedTool, official_binary_name};
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

/// A resolved official artifact ready for native (host or robot) staging. The
/// public identity is the provider-qualified `package` id
/// (`phoxal/service-drive`); on-disk names use its filesystem-safe projection
/// ([`Self::package`]/[`Self::tag`]) since a package id's `/` is not a legal
/// path component (docs #21).
///
/// Contract/config metadata is no longer part of staging at all: the catalog
/// inlines it in the manifest's `ArtifactEntry` (`resolver::ResolvedPlatformRuntime::contracts`
/// etc.), so there is no `.emit-apis.json` sidecar to fetch here anymore -
/// staging is purely "download the tarball, verify it, unpack it."
#[derive(Debug, Clone)]
pub struct NativeArtifactDescriptor {
    pub package_id: String,
    pub kind: ArtifactKind,
    pub name: String,
    pub version: String,
    pub asset: String,
    pub sha256: String,
    /// The binary name inside the unpacked tarball. Empty for
    /// `component_assets` - an asset bundle has no binary.
    pub binary_name: String,
    /// The API generation this artifact authors against; empty for
    /// `component_assets`. Carried here (duplicating
    /// `ResolvedPlatformRuntime`/`ResolvedTool`) so [`stage_descriptor`] can
    /// upsert the local download manifest entry on its own, without every
    /// caller threading this metadata back through separately.
    pub generation: String,
    pub contracts: Vec<crate::catalog::Contract>,
    pub config_schema: Option<serde_json::Value>,
    pub bus_abi: Option<String>,
    pub changed_contracts: Vec<String>,
    pub channel: crate::catalog::Channel,
    /// The target triple this tarball was resolved/built for, or
    /// [`crate::catalog::TARGET_INDEPENDENT_SCOPE`] for `component_assets`.
    pub target: String,
}

impl NativeArtifactDescriptor {
    /// Build a descriptor from any resolved official artifact that carries a
    /// built tarball: a service/simulator ([`ResolvedPlatformRuntime`]) or -
    /// via [`crate::resolver::ResolvedComponentPackage::catalog_runtime`] - a
    /// catalog-resolved component assets bundle or driver, since both project
    /// onto the identical `ResolvedPlatformRuntime` shape. Returns `None` when
    /// there is nothing to stage (a path override, or a catalog entry with no
    /// built artifact for this scope yet).
    pub fn from_runtime(runtime: &ResolvedPlatformRuntime) -> Result<Option<Self>> {
        let Some(sha256) = &runtime.sha256 else {
            return Ok(None);
        };
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
            asset: runtime.artifact_ref().to_string(),
            sha256: sha256.clone(),
            binary_name,
            generation: runtime.generation.clone(),
            contracts: runtime.contracts.clone(),
            config_schema: runtime.config_schema.clone(),
            bus_abi: runtime.bus_abi.clone(),
            changed_contracts: runtime.changed_contracts.clone(),
            channel: runtime.channel,
            target: runtime.target.clone(),
        }))
    }

    pub fn from_tool(tool: &ResolvedTool) -> Result<Option<Self>> {
        if !tool.published || tool.path_override.is_some() {
            return Ok(None);
        }
        Ok(Some(Self {
            package_id: tool.package.clone(),
            kind: ArtifactKind::Tool,
            name: crate::resolver::tool_emit_apis_id(&tool.name).to_string(),
            version: tool.resolved.clone(),
            asset: tool.asset.clone(),
            sha256: tool.sha256.clone(),
            binary_name: tool.binary_name.clone(),
            generation: tool.generation.clone(),
            contracts: tool.contracts.clone(),
            config_schema: tool.config_schema.clone(),
            bus_abi: tool.bus_abi.clone(),
            changed_contracts: Vec::new(),
            channel: tool.channel,
            target: tool.target.clone(),
        }))
    }

    /// The filesystem-safe projection of the provider-qualified package id
    /// (`phoxal/service-drive` -> `phoxal-service-drive`) used for cache
    /// directory names, release tags, and asset file names.
    #[must_use]
    pub fn package(&self) -> String {
        self.package_id.replace('/', "-")
    }

    #[must_use]
    pub fn tag(&self) -> String {
        format!("{}-v{}", self.package(), self.version)
    }

    fn local_manifest_upsert(&self) -> LocalManifestUpsert {
        LocalManifestUpsert {
            kind: self.kind,
            package: self.package_id.clone(),
            version: self.version.clone(),
            generation: self.generation.clone(),
            contracts: self.contracts.clone(),
            config_schema: self.config_schema.clone(),
            bus_abi: self.bus_abi.clone(),
            changed_contracts: self.changed_contracts.clone(),
            channel: self.channel,
            target: self.target.clone(),
            tarball: self.asset.clone(),
            sha256: self.sha256.clone(),
        }
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

/// Stage a resolved component package's (assets or driver) catalog bundle.
/// `None` when the package is not catalog-sourced (`Path`/`Git` - a local
/// override with no bundle to fetch) or when the catalog entry has no built
/// artifact for the needed scope yet. Reuses the identical
/// [`NativeArtifactDescriptor`]/[`stage_descriptor`] machinery services and
/// tools already stage through - a component's `catalog_runtime` projects onto
/// the same [`ResolvedPlatformRuntime`] shape.
pub fn stage_component_package(
    ui: Option<&Ui>,
    package: &crate::resolver::ResolvedComponentPackage,
    mode: ProvisioningMode,
) -> Result<Option<PathBuf>> {
    let Some(runtime) = &package.catalog_runtime else {
        return Ok(None);
    };
    stage_runtime(ui, runtime, mode)
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
    // One component id may back several instances (`left_drive`/`right_drive`
    // both resolve `phoxal/component-ddsm115-*`); stage each distinct package
    // once. Path/Git-sourced packages have no `catalog_runtime` and are
    // skipped here - they already have a local source directory.
    let mut staged_packages = std::collections::BTreeSet::new();
    for component in &resolved.components {
        if staged_packages.insert(component.assets.package.clone())
            && stage_component_package(ui, &component.assets, mode)?.is_some()
        {
            count += 1;
        }
        if let Some(driver) = &component.driver
            && staged_packages.insert(driver.package.clone())
            && stage_component_package(ui, driver, mode)?.is_some()
        {
            count += 1;
        }
    }
    Ok(count)
}

/// Stage every resolved component's `component_assets` bundle
/// (`component.yaml`, `structure.urdf`, `simulation.yaml`, `meshes/`) into
/// `<robot_root>/components/<component-id>/`, so `robot_root`-relative asset
/// resolution (`PHOXAL_ROBOT_ROOT` + `PHOXAL_COMPONENT_INSTANCE`, see
/// `phoxal::participant::launch`) finds the same shape for `run`/`simulate`
/// that deploy already stages under `/opt/phoxal/components/` (docs #21). One
/// component id may back several instances; its bundle is copied once and
/// shared. A no-op when the resolved source directory already IS
/// `<robot_root>/components/<id>` (the common `Path`-pinned dev-overlay case),
/// since only a `Catalog`/`Git` resolution whose files live elsewhere needs an
/// actual copy.
pub fn stage_component_bundles_into_robot_root(
    project_root: &Path,
    robot_root: &Path,
    resolved: &crate::resolver::ResolvedRobot,
) -> Result<()> {
    let mut staged = std::collections::BTreeSet::new();
    for component in &resolved.components {
        let component_id = &component.source_name;
        if !staged.insert(component_id.clone()) {
            continue;
        }
        let source_dir = crate::component_driver::component_assets_dir(component, project_root)
            .with_context(|| format!("failed to locate component assets for '{component_id}'"))?;
        let dest_dir = robot_root.join("components").join(component_id);
        if source_dir == dest_dir {
            continue;
        }
        copy_component_bundle_files(&source_dir, &dest_dir)?;
    }
    Ok(())
}

/// Copy one component's asset bundle files from `source_dir` into `dest_dir`.
/// `component.yaml` is required; `structure.urdf`/`simulation.yaml`/`meshes/`
/// are optional per component.
fn copy_component_bundle_files(source_dir: &Path, dest_dir: &Path) -> Result<()> {
    const COMPONENT_FILE: &str = "component.yaml";
    const OPTIONAL_FILES: [&str; 2] = ["structure.urdf", "simulation.yaml"];
    const MESHES_DIR: &str = "meshes";

    fs::create_dir_all(dest_dir)
        .with_context(|| format!("failed to create {}", dest_dir.display()))?;

    let component_file = source_dir.join(COMPONENT_FILE);
    fs::copy(&component_file, dest_dir.join(COMPONENT_FILE)).with_context(|| {
        format!(
            "failed to stage component metadata {} to {}",
            component_file.display(),
            dest_dir.display()
        )
    })?;

    for optional_file in OPTIONAL_FILES {
        let source_file = source_dir.join(optional_file);
        if !source_file.is_file() {
            continue;
        }
        fs::copy(&source_file, dest_dir.join(optional_file)).with_context(|| {
            format!(
                "failed to stage {} to {}",
                source_file.display(),
                dest_dir.display()
            )
        })?;
    }

    let meshes_source = source_dir.join(MESHES_DIR);
    if meshes_source.is_dir() {
        copy_dir_recursive_into(&meshes_source, &dest_dir.join(MESHES_DIR))?;
    }
    Ok(())
}

fn copy_dir_recursive_into(source: &Path, dest: &Path) -> Result<()> {
    fs::create_dir_all(dest).with_context(|| format!("failed to create {}", dest.display()))?;
    for entry in
        fs::read_dir(source).with_context(|| format!("failed to read {}", source.display()))?
    {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let source_path = entry.path();
        let dest_path = dest.join(entry.file_name());
        if file_type.is_dir() {
            copy_dir_recursive_into(&source_path, &dest_path)?;
        } else if file_type.is_file() {
            fs::copy(&source_path, &dest_path).with_context(|| {
                format!(
                    "failed to stage {} to {}",
                    source_path.display(),
                    dest_path.display()
                )
            })?;
        }
    }
    Ok(())
}

/// Stage one native artifact: ensure its tarball is present in the flat
/// content-addressed cache ([`artifact_tarball_path`]), then lazily unpack it
/// into its ephemeral `run/exec/` location ([`artifact_exec_dir`]).
/// `component_assets` descriptors have no binary at all, so "staged" just
/// means the tarball is unpacked.
pub fn stage_descriptor(
    ui: Option<&Ui>,
    descriptor: &NativeArtifactDescriptor,
    mode: ProvisioningMode,
) -> Result<PathBuf> {
    let exec_dir = artifact_exec_dir(descriptor)?;
    let binary = exec_dir.join(&descriptor.binary_name);
    let tarball_path = ensure_tarball_cached(descriptor, mode)?;
    if mode == ProvisioningMode::MissingOnly && exec_dir.is_dir() {
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
    unpack_asset(&tarball_path, &exec_dir)?;
    if binary.is_file() {
        make_executable(&binary)?;
    }
    Ok(binary)
}

/// Ensure `descriptor`'s tarball is present and sha256-valid in the flat
/// `cache/artifacts/` store, downloading it if missing/mismatched/`Refresh`d.
/// The tarball file itself IS the cache entry - presence plus a matching
/// sha256 means "skip re-download". On a fresh download, upserts the local
/// download manifest ([`crate::catalog::upsert_local_manifest_entry`]) with
/// this artifact's inlined contracts/config/bus_abi so `check`/`run` can read
/// them back offline for this cached artifact.
fn ensure_tarball_cached(
    descriptor: &NativeArtifactDescriptor,
    mode: ProvisioningMode,
) -> Result<PathBuf> {
    let path = artifact_tarball_path(descriptor)?;
    let cached_and_valid = mode != ProvisioningMode::Refresh
        && path.is_file()
        && sha256_file(&path).is_ok_and(|sha| sha == descriptor.sha256);
    if !cached_and_valid {
        let bytes =
            download_release_asset(&descriptor.tag(), &descriptor.asset, &descriptor.sha256)?;
        write_file_atomic(&path, &bytes)?;
        crate::catalog::upsert_local_manifest_entry(descriptor.local_manifest_upsert())
            .context("failed to update the local download manifest")?;
    }
    verify_file_sha256(&path, &descriptor.sha256)?;
    Ok(path)
}

pub fn artifact_binary_path(descriptor: &NativeArtifactDescriptor) -> Result<PathBuf> {
    Ok(artifact_exec_dir(descriptor)?.join(&descriptor.binary_name))
}

/// The flat, content-addressed tarball cache path: `cache/artifacts/<asset>`,
/// named exactly as the published release asset - no per-package
/// subdirectories.
pub fn artifact_tarball_path(descriptor: &NativeArtifactDescriptor) -> Result<PathBuf> {
    Ok(crate::host_paths::artifacts_dir()?.join(&descriptor.asset))
}

/// Where `descriptor`'s tarball is lazily unpacked for use: an ephemeral
/// location under `run/exec/`, NOT under `cache/artifacts/` - the cache stays
/// a pure tarball store, and the unpacked bundle is disposable (it can always
/// be rebuilt from the cached tarball). Keyed by the tarball's own filename
/// (already a unique, filesystem-safe identity for this exact artifact/
/// version/target), so re-staging the same descriptor reuses the same dir.
pub fn artifact_exec_dir(descriptor: &NativeArtifactDescriptor) -> Result<PathBuf> {
    Ok(crate::host_paths::run_dir()?
        .join("exec")
        .join(exec_dir_name(&descriptor.asset)))
}

fn exec_dir_name(asset: &str) -> String {
    let stem = asset
        .strip_suffix(".tar.zst")
        .or_else(|| asset.strip_suffix(".tar.gz"))
        .or_else(|| asset.strip_suffix(".tar"))
        .unwrap_or(asset);
    sanitize_path_segment(stem)
}

fn release_asset_url(tag: &str, asset: &str) -> String {
    format!("https://github.com/{FRAMEWORK_REPO}/releases/download/{tag}/{asset}")
}

fn download_release_asset(tag: &str, asset: &str, expected_sha256: &str) -> Result<Vec<u8>> {
    let url = release_asset_url(tag, asset);
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
            "download of {asset} from {tag} returned {}",
            response.status()
        );
    }
    let bytes = response
        .bytes()
        .with_context(|| format!("failed to read {asset} body"))?
        .to_vec();
    let actual = hex::encode(Sha256::digest(&bytes));
    if actual != expected_sha256 {
        bail!("sha256 mismatch for {asset}: expected {expected_sha256}, got {actual}");
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
    let parent = path
        .parent()
        .context("artifact cache path did not have a parent directory")?;
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
