use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use flate2::read::GzDecoder;
use sha2::{Digest, Sha256};
use tar::Archive;

use crate::catalog::ArtifactKind;
use crate::resolver::{ResolvedPlatformRuntime, ResolvedTool, official_binary_name};
use crate::ui::Ui;
use crate::utils::make_executable;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProvisioningMode {
    MissingOnly,
}

/// A resolved official artifact ready for native (host or robot) staging. The
/// public identity is the provider-qualified `package` id
/// (`phoxal/service-drive`); on-disk names use its filesystem-safe projection
/// ([`Self::package`]/[`Self::tag`]) since a package id's `/` is not a legal
/// path component (docs #21).
///
/// Catalog staging is location/integrity-only. Participant metadata is read
/// from the unpacked binary after staging.
#[derive(Debug, Clone)]
pub struct NativeArtifactDescriptor {
    pub package_id: String,
    pub kind: ArtifactKind,
    pub name: String,
    pub version: String,
    pub url: String,
    pub sha256: String,
    pub size: u64,
    /// The binary name inside the unpacked tarball. Empty for
    /// `component_assets` - an asset bundle has no binary.
    pub binary_name: String,
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
            kind: ArtifactKind::Tool,
            name: crate::resolver::tool_emit_apis_id(&tool.name).to_string(),
            version: tool.resolved.clone(),
            url: tool.url.clone().unwrap_or_default(),
            sha256: tool.sha256.clone(),
            size: tool.size.unwrap_or_default(),
            binary_name: tool.binary_name.clone(),
            target: tool.target.clone(),
        }))
    }

    /// The validated, collision-free local directory key for this package.
    pub fn package(&self) -> Result<String> {
        package_storage_key(&self.package_id)
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

pub fn descriptors(
    resolved: &crate::resolver::ResolvedRobot,
) -> Result<Vec<NativeArtifactDescriptor>> {
    let mut descriptors = Vec::new();
    for runtime in resolved
        .platform_runtimes
        .iter()
        .chain(&resolved.simulators)
    {
        if let Some(descriptor) = NativeArtifactDescriptor::from_runtime(runtime)? {
            descriptors.push(descriptor);
        }
    }
    for tool in &resolved.tools {
        if let Some(descriptor) = NativeArtifactDescriptor::from_tool(tool)? {
            descriptors.push(descriptor);
        }
    }
    let mut components = std::collections::BTreeSet::new();
    for component in &resolved.components {
        for package in std::iter::once(&component.assets).chain(component.driver.as_ref()) {
            if let Some(runtime) = &package.catalog_runtime
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

/// Stage one native artifact in the project-local version directory. Missing
/// blobs are downloaded and verified before unpacking; existing versions are
/// reused. Component-asset descriptors have no binary, so staging only
/// ensures the bundle is unpacked.
pub fn stage_descriptor(
    ui: Option<&Ui>,
    descriptor: &NativeArtifactDescriptor,
    mode: ProvisioningMode,
) -> Result<PathBuf> {
    let _lock = ArtifactStoreLock::shared()?;
    let binary = prepare_descriptor(ui, descriptor, mode)?;
    retarget_active(descriptor)?;
    Ok(binary)
}

pub struct ArtifactStoreLock {
    file: fs::File,
    holder_path: Option<PathBuf>,
}

impl ArtifactStoreLock {
    pub fn exclusive(command: &str) -> Result<Self> {
        Self::acquire(true, command)
    }

    fn shared() -> Result<Self> {
        Self::acquire(false, "staging")
    }

    fn acquire(exclusive: bool, command: &str) -> Result<Self> {
        let state = crate::host_paths::project_state_dir()?;
        fs::create_dir_all(&state)?;
        let path = state.join("artifacts.lock");
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)?;
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            let operation = if exclusive {
                libc::LOCK_EX
            } else {
                libc::LOCK_SH
            } | libc::LOCK_NB;
            if unsafe { libc::flock(file.as_raw_fd(), operation) } != 0 {
                let holder = fs::read_to_string(state.join("artifacts.lock.holder"))
                    .unwrap_or_else(|_| "unknown holder".to_string());
                bail!("artifact store lock is held ({})", holder.trim());
            }
        }
        let holder_path = exclusive.then(|| state.join("artifacts.lock.holder"));
        if let Some(holder_path) = &holder_path {
            fs::write(
                holder_path,
                format!("pid={} command={command}\n", std::process::id()),
            )?;
        }
        Ok(Self { file, holder_path })
    }
}

impl Drop for ArtifactStoreLock {
    fn drop(&mut self) {
        if let Some(path) = &self.holder_path {
            fs::remove_file(path).ok();
        }
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
        }
    }
}

fn prepare_descriptor(
    ui: Option<&Ui>,
    descriptor: &NativeArtifactDescriptor,
    mode: ProvisioningMode,
) -> Result<PathBuf> {
    let version_dir = artifact_exec_dir(descriptor)?;
    let binary = version_dir.join(&descriptor.binary_name);
    if mode == ProvisioningMode::MissingOnly && version_dir.is_dir() {
        return Ok(binary);
    }
    if descriptor.url.is_empty() {
        bail!(
            "vendored {} {} for target {} is missing; run `phoxal update` online",
            descriptor.package_id,
            descriptor.version,
            descriptor.target
        );
    }

    if let Some(ui) = ui {
        ui.info(format!(
            "provisioning {} {} from {}",
            descriptor.kind, descriptor.name, descriptor.url
        ));
    }
    let tarball_path = download_blob(descriptor)?;
    unpack_asset(&tarball_path, &version_dir)?;
    fs::remove_file(&tarball_path).ok();
    if binary.is_file() {
        make_executable(&binary)?;
    }
    Ok(binary)
}

/// Download/unpack every descriptor with bounded concurrency, then retarget
/// all active links only after every blob verified successfully.
pub fn prepare_and_activate_descriptors(
    descriptors: &[NativeArtifactDescriptor],
    ui: Option<&Ui>,
) -> Result<()> {
    const CONCURRENCY: usize = 4;
    for batch in descriptors.chunks(CONCURRENCY) {
        std::thread::scope(|scope| -> Result<()> {
            let handles = batch
                .iter()
                .map(|descriptor| {
                    scope.spawn(move || {
                        prepare_descriptor(None, descriptor, ProvisioningMode::MissingOnly)
                    })
                })
                .collect::<Vec<_>>();
            for handle in handles {
                handle
                    .join()
                    .map_err(|_| anyhow!("artifact download worker panicked"))??;
            }
            Ok(())
        })?;
        if let Some(ui) = ui {
            for descriptor in batch {
                ui.info(format!(
                    "verified {} {} [{}] ({} bytes)",
                    descriptor.package_id, descriptor.version, descriptor.target, descriptor.size
                ));
            }
        }
    }
    for descriptor in descriptors {
        retarget_active(descriptor)?;
    }
    Ok(())
}

/// Return the selected binary through its `(package, target)/active` symlink.
pub fn artifact_binary_path(descriptor: &NativeArtifactDescriptor) -> Result<PathBuf> {
    let active = artifact_target_dir(descriptor)?.join("active");
    Ok(active.join(&descriptor.binary_name))
}

/// Temporary download path beside the selected version directory.
pub fn artifact_tarball_path(descriptor: &NativeArtifactDescriptor) -> Result<PathBuf> {
    Ok(artifact_target_dir(descriptor)?.join(format!(".{}.partial", descriptor.version)))
}

/// Where `descriptor` is unpacked in the project-local binary store.
pub fn artifact_exec_dir(descriptor: &NativeArtifactDescriptor) -> Result<PathBuf> {
    validate_path_segment("artifact version", &descriptor.version)?;
    Ok(artifact_target_dir(descriptor)?.join(&descriptor.version))
}

fn artifact_target_dir(descriptor: &NativeArtifactDescriptor) -> Result<PathBuf> {
    artifact_target_dir_for(&descriptor.package_id, &descriptor.target)
}

pub fn artifact_target_dir_for(package: &str, target: &str) -> Result<PathBuf> {
    validate_path_segment("artifact target", target)?;
    Ok(crate::host_paths::binaries_dir()?
        .join(package_storage_key(package)?)
        .join(target))
}

pub fn active_version(descriptor: &NativeArtifactDescriptor) -> Result<Option<String>> {
    let active = artifact_target_dir(descriptor)?.join("active");
    match fs::read_link(&active) {
        Ok(target) => Ok(target
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("failed to read {}", active.display())),
    }
}

pub fn count_versions() -> Result<usize> {
    walk_artifact_targets(false, false, None).map(|(retained, _)| retained)
}

pub fn prune_inactive_versions(current: &[NativeArtifactDescriptor]) -> Result<(usize, usize)> {
    classify_inactive_versions(current, true)
}

pub fn preview_prune_inactive_versions(
    current: &[NativeArtifactDescriptor],
) -> Result<(usize, usize)> {
    classify_inactive_versions(current, false)
}

fn classify_inactive_versions(
    current: &[NativeArtifactDescriptor],
    remove: bool,
) -> Result<(usize, usize)> {
    let packages = current
        .iter()
        .map(NativeArtifactDescriptor::package)
        .collect::<Result<std::collections::BTreeSet<_>>>()?;
    walk_artifact_targets(true, remove, Some(&packages))
}

fn walk_artifact_targets(
    classify_inactive: bool,
    remove: bool,
    current_packages: Option<&std::collections::BTreeSet<String>>,
) -> Result<(usize, usize)> {
    let root = crate::host_paths::binaries_dir()?;
    if !root.is_dir() {
        return Ok((0, 0));
    }
    let mut retained = 0;
    let mut pruned = 0;
    for package in fs::read_dir(&root)? {
        let package = package?;
        if !package.file_type()?.is_dir() {
            continue;
        }
        if classify_inactive
            && current_packages.is_some_and(|packages| {
                !packages.contains(&package.file_name().to_string_lossy().into_owned())
            })
        {
            for target in fs::read_dir(package.path())? {
                let target = target?;
                if target.file_type()?.is_dir() {
                    pruned += fs::read_dir(target.path())?
                        .filter_map(Result::ok)
                        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
                        .count();
                }
            }
            if remove {
                fs::remove_dir_all(package.path())?;
            }
            continue;
        }
        for target in fs::read_dir(package.path())? {
            let target = target?;
            if !target.file_type()?.is_dir() {
                continue;
            }
            let active = fs::read_link(target.path().join("active"))
                .ok()
                .and_then(|path| path.file_name().map(|name| name.to_os_string()));
            for version in fs::read_dir(target.path())? {
                let version = version?;
                if !version.file_type()?.is_dir() {
                    continue;
                }
                if active
                    .as_ref()
                    .is_some_and(|active| active == &version.file_name())
                {
                    retained += 1;
                } else if classify_inactive {
                    if remove {
                        fs::remove_dir_all(version.path())?;
                    }
                    pruned += 1;
                } else {
                    retained += 1;
                }
            }
        }
    }
    Ok((retained, pruned))
}

pub fn existing_target_scopes(package: &str) -> Result<Vec<String>> {
    let package_dir = crate::host_paths::binaries_dir()?.join(package_storage_key(package)?);
    if !package_dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut targets = Vec::new();
    for entry in fs::read_dir(package_dir)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            let target = entry.file_name().to_string_lossy().into_owned();
            validate_path_segment("stored artifact target", &target)?;
            targets.push(target);
        }
    }
    targets.sort();
    Ok(targets)
}

fn download_blob(descriptor: &NativeArtifactDescriptor) -> Result<PathBuf> {
    let url = &descriptor.url;
    let client = reqwest::blocking::Client::builder()
        .user_agent("phoxal-cli")
        .timeout(Duration::from_secs(120))
        .build()?;
    let response = client
        .get(url)
        .send()
        .with_context(|| format!("failed to download {url}"))?;
    if !response.status().is_success() {
        bail!("download from {url} returned {}", response.status());
    }
    let bytes = response
        .bytes()
        .with_context(|| format!("failed to read {url} body"))?
        .to_vec();
    verify_blob_bytes(descriptor, &bytes)?;
    let path = artifact_tarball_path(descriptor)?;
    write_file_atomic(&path, &bytes)?;
    Ok(path)
}

fn verify_blob_bytes(descriptor: &NativeArtifactDescriptor, bytes: &[u8]) -> Result<()> {
    if bytes.len() as u64 != descriptor.size {
        bail!(
            "size mismatch for {} {}: expected {} bytes, got {}",
            descriptor.package_id,
            descriptor.version,
            descriptor.size,
            bytes.len()
        );
    }
    let actual = hex::encode(Sha256::digest(bytes));
    if actual != descriptor.sha256 {
        bail!(
            "sha256 mismatch for {}: expected {}, got {actual}",
            descriptor.package_id,
            descriptor.sha256
        );
    }
    Ok(())
}

fn retarget_active(descriptor: &NativeArtifactDescriptor) -> Result<()> {
    let target_dir = artifact_target_dir(descriptor)?;
    fs::create_dir_all(&target_dir)?;
    let partial = target_dir.join(".active.partial");
    if fs::symlink_metadata(&partial).is_ok() {
        fs::remove_file(&partial)?;
    }
    #[cfg(unix)]
    std::os::unix::fs::symlink(&descriptor.version, &partial)?;
    #[cfg(windows)]
    std::os::windows::fs::symlink_dir(&descriptor.version, &partial)?;
    fs::rename(&partial, target_dir.join("active"))
        .context("failed to atomically retarget the active artifact version")
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

fn package_storage_key(package: &str) -> Result<String> {
    let segments = package.split('/').collect::<Vec<_>>();
    anyhow::ensure!(
        segments.len() == 2,
        "artifact package must be provider-qualified as <provider>/<name>: {package:?}"
    );
    for segment in &segments {
        validate_path_segment("artifact package segment", segment)?;
    }
    Ok(format!("{}%2F{}", segments[0], segments[1]))
}

fn validate_path_segment(label: &str, value: &str) -> Result<()> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host_paths::test_support::ScratchPhoxalHome;

    fn descriptor(version: &str, bytes: &[u8]) -> NativeArtifactDescriptor {
        NativeArtifactDescriptor {
            package_id: "phoxal/service-drive".to_string(),
            kind: ArtifactKind::Service,
            name: "drive".to_string(),
            version: version.to_string(),
            url: "https://example.invalid/drive.tar".to_string(),
            sha256: hex::encode(Sha256::digest(bytes)),
            size: bytes.len() as u64,
            binary_name: "phoxal-service-drive".to_string(),
            target: "aarch64-unknown-linux-musl".to_string(),
        }
    }

    #[test]
    fn blob_size_and_sha_are_both_enforced() {
        let bytes = b"verified";
        let descriptor = descriptor("1.0.0", bytes);
        verify_blob_bytes(&descriptor, bytes).unwrap();
        assert!(verify_blob_bytes(&descriptor, b"wrong").is_err());
        let mut wrong_sha = descriptor;
        wrong_sha.sha256 = "0".repeat(64);
        assert!(verify_blob_bytes(&wrong_sha, bytes).is_err());
    }

    #[test]
    fn active_symlink_selects_one_version_and_pruning_keeps_it() -> Result<()> {
        let _root = ScratchPhoxalHome::new()?;
        let old = descriptor("1.0.0", b"old");
        let new = descriptor("2.0.0", b"new");
        fs::create_dir_all(artifact_exec_dir(&old)?)?;
        fs::create_dir_all(artifact_exec_dir(&new)?)?;
        retarget_active(&new)?;
        assert_eq!(active_version(&new)?.as_deref(), Some("2.0.0"));
        let (retained, pruned) = prune_inactive_versions(std::slice::from_ref(&new))?;
        assert_eq!((retained, pruned), (1, 1));
        assert!(artifact_exec_dir(&new)?.is_dir());
        assert!(!artifact_exec_dir(&old)?.exists());
        Ok(())
    }

    #[test]
    fn prune_preview_reports_without_mutating() -> Result<()> {
        let _root = ScratchPhoxalHome::new()?;
        let old = descriptor("1.0.0", b"old");
        let new = descriptor("2.0.0", b"new");
        fs::create_dir_all(artifact_exec_dir(&old)?)?;
        fs::create_dir_all(artifact_exec_dir(&new)?)?;
        retarget_active(&new)?;

        assert_eq!(
            preview_prune_inactive_versions(std::slice::from_ref(&new))?,
            (1, 1)
        );
        assert!(artifact_exec_dir(&old)?.is_dir());
        assert!(artifact_exec_dir(&new)?.is_dir());
        Ok(())
    }

    #[test]
    fn local_identity_is_validated_and_collision_free() -> Result<()> {
        assert_eq!(
            package_storage_key("phoxal/service-drive")?,
            "phoxal%2Fservice-drive"
        );
        assert_ne!(
            package_storage_key("phoxal/service-drive")?,
            "phoxal-service-drive"
        );
        assert!(package_storage_key("../service-drive").is_err());
        assert!(package_storage_key("phoxal/service/drive").is_err());

        let mut invalid = descriptor("../escape", b"bytes");
        assert!(artifact_exec_dir(&invalid).is_err());
        invalid.version = "1.0.0".to_string();
        invalid.target = "../../escape".to_string();
        assert!(artifact_exec_dir(&invalid).is_err());
        Ok(())
    }
}
