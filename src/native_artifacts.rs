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
    /// The target triple this tarball was resolved/built for. `None` identifies
    /// the catalog's distinct component-assets blob.
    pub target: Option<String>,
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
            target: Some(tool.target.clone()),
        }))
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
    _mode: ProvisioningMode,
) -> Result<usize> {
    let descriptors = descriptors_for(resolved, true, true)?;
    prepare_descriptors_with_preflight(&descriptors, ui)?;
    Ok(descriptors.len())
}

pub fn descriptors(
    resolved: &crate::resolver::ResolvedRobot,
) -> Result<Vec<NativeArtifactDescriptor>> {
    descriptors_for(resolved, true, true)
}

pub fn descriptors_for(
    resolved: &crate::resolver::ResolvedRobot,
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

pub fn prepare_descriptors_with_preflight(
    descriptors: &[NativeArtifactDescriptor],
    ui: Option<&Ui>,
) -> Result<()> {
    let actionable = descriptors
        .iter()
        .filter(|descriptor| should_prepare_descriptor(descriptor))
        .cloned()
        .collect::<Vec<_>>();
    let missing = actionable
        .iter()
        .filter(|descriptor| artifact_exec_dir(descriptor).is_ok_and(|path| !path.is_dir()))
        .collect::<Vec<_>>();
    if !missing.is_empty() {
        let total_bytes = missing
            .iter()
            .map(|descriptor| descriptor.size)
            .sum::<u64>();
        let destination = crate::host_paths::artifacts_dir()?;
        let free = free_disk_bytes(&destination).ok();
        if let Some(ui) = ui {
            ui.info(format!(
                "artifact preflight: {} package target(s), {} bytes, destination {}",
                missing.len(),
                total_bytes,
                destination.display()
            ));
            if let Some(free) = free {
                ui.info(format!("free disk: {free} bytes"));
            }
        }
        if let Some(free) = free
            && total_bytes > free
        {
            bail!(
                "artifact download needs {total_bytes} bytes but only {free} bytes are free at {}; run `phoxal cache clean --artifacts` or free disk space",
                destination.display()
            );
        }
    }
    if actionable.is_empty() {
        return Ok(());
    }
    let _lock = ArtifactStoreLock::exclusive("provision")?;
    if missing.is_empty() {
        // Finding A3: every actionable descriptor is already staged (a warm
        // cache) - only cheap activation/retargeting remains, which is not
        // itself download work, so no "download" phase appears (Product
        // decision 3).
        return prepare_and_activate_descriptors(&actionable, ui);
    }
    let count = missing.len();
    crate::session::diagnostics::run_phase(
        crate::session::event::PhaseId::new("download"),
        format!(
            "Downloading {count} artifact package{}",
            if count == 1 { "" } else { "s" }
        ),
        || prepare_and_activate_descriptors(&actionable, ui),
    )
}

fn should_prepare_descriptor(descriptor: &NativeArtifactDescriptor) -> bool {
    if descriptor.url.is_empty() {
        return false;
    }
    #[cfg(test)]
    if descriptor.url.starts_with("https://example.invalid/") {
        return false;
    }
    true
}

#[cfg(unix)]
// `statvfs` field widths vary by platform (`f_bavail` is u32 on macOS, u64 on
// Linux), so the `u64::from` below is a real widening on some targets and a
// no-op on others; allow the lint rather than pick a cast that only moves it.
#[allow(clippy::useless_conversion)]
fn free_disk_bytes(path: &Path) -> Result<u64> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let existing = path
        .ancestors()
        .find(|candidate| candidate.exists())
        .context("artifact destination has no existing ancestor")?;
    let path = CString::new(existing.as_os_str().as_bytes())?;
    let mut stats = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    if unsafe { libc::statvfs(path.as_ptr(), stats.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let stats = unsafe { stats.assume_init() };
    Ok(u64::from(stats.f_bavail).saturating_mul(stats.f_frsize))
}

#[cfg(not(unix))]
fn free_disk_bytes(_path: &Path) -> Result<u64> {
    bail!("free-disk reporting is unavailable on this platform")
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
    let mut bundles = Vec::new();
    for component in &resolved.components {
        let component_id = &component.source_name;
        if !staged.insert(component_id.clone()) {
            continue;
        }
        let Some(source_dir) =
            crate::component_driver::component_assets_dir(component, project_root).with_context(
                || format!("failed to locate component assets for '{component_id}'"),
            )?
        else {
            // Driverless (passive) component with no official assets
            // package - nothing to stage.
            continue;
        };
        let dest_dir = robot_root.join("components").join(component_id);
        if source_dir == dest_dir {
            continue;
        }
        bundles.push((source_dir, dest_dir));
    }
    let _lock = crate::host_paths::artifacts_dir()
        .is_ok_and(|path| path.is_dir())
        .then(ArtifactStoreLock::shared)
        .transpose()?;
    for (source_dir, dest_dir) in bundles {
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
    let _lock = ArtifactStoreLock::exclusive("provision")?;
    let binary = prepare_descriptor(ui, descriptor, mode)?;
    retarget_active(descriptor)?;
    Ok(binary)
}

pub struct ArtifactStoreLock {
    file: fs::File,
    path: PathBuf,
    exclusive: bool,
}

impl ArtifactStoreLock {
    pub fn exclusive(command: &str) -> Result<Self> {
        Self::acquire(true, command)
    }

    pub fn shared() -> Result<Self> {
        Self::acquire(false, "staging")
    }

    fn acquire(exclusive: bool, command: &str) -> Result<Self> {
        let store = crate::host_paths::artifacts_dir()?;
        fs::create_dir_all(&store)?;
        let path = store.join(".lock");
        let mut options = fs::OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            const FILE_FLAG_DELETE_ON_CLOSE: u32 = 0x0400_0000;
            options.custom_flags(FILE_FLAG_DELETE_ON_CLOSE);
        }
        let file = options.open(&path)?;
        try_advisory_lock(&file, exclusive)
            .with_context(|| format!("artifact store lock is held (requested by {command})"))?;
        Ok(Self {
            file,
            path,
            exclusive,
        })
    }
}

impl Drop for ArtifactStoreLock {
    fn drop(&mut self) {
        if !self.exclusive {
            let _ = unlock_advisory(&self.file);
            if try_advisory_lock(&self.file, true).is_err() {
                return;
            }
        }
        fs::remove_file(&self.path).ok();
        let _ = unlock_advisory(&self.file);
    }
}

#[cfg(unix)]
fn try_advisory_lock(file: &fs::File, exclusive: bool) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;

    let operation = if exclusive {
        libc::LOCK_EX
    } else {
        libc::LOCK_SH
    } | libc::LOCK_NB;
    if unsafe { libc::flock(file.as_raw_fd(), operation) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(unix)]
fn unlock_advisory(file: &fs::File) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;

    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(windows)]
fn try_advisory_lock(file: &fs::File, exclusive: bool) -> std::io::Result<()> {
    use std::ffi::c_void;
    use std::os::windows::io::AsRawHandle;

    const LOCKFILE_FAIL_IMMEDIATELY: u32 = 0x0000_0001;
    const LOCKFILE_EXCLUSIVE_LOCK: u32 = 0x0000_0002;
    let flags = LOCKFILE_FAIL_IMMEDIATELY
        | if exclusive {
            LOCKFILE_EXCLUSIVE_LOCK
        } else {
            0
        };
    let mut overlapped = WindowsOverlapped::default();
    let result = unsafe {
        LockFileEx(
            file.as_raw_handle().cast::<c_void>(),
            flags,
            0,
            u32::MAX,
            u32::MAX,
            &mut overlapped,
        )
    };
    if result != 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(windows)]
fn unlock_advisory(file: &fs::File) -> std::io::Result<()> {
    use std::ffi::c_void;
    use std::os::windows::io::AsRawHandle;

    let mut overlapped = WindowsOverlapped::default();
    let result = unsafe {
        UnlockFileEx(
            file.as_raw_handle().cast::<c_void>(),
            0,
            u32::MAX,
            u32::MAX,
            &mut overlapped,
        )
    };
    if result != 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(windows)]
#[repr(C)]
#[derive(Default)]
struct WindowsOverlapped {
    internal: usize,
    internal_high: usize,
    offset: u32,
    offset_high: u32,
    event: *mut std::ffi::c_void,
}

#[cfg(windows)]
#[link(name = "kernel32")]
unsafe extern "system" {
    fn LockFileEx(
        file: *mut std::ffi::c_void,
        flags: u32,
        reserved: u32,
        bytes_low: u32,
        bytes_high: u32,
        overlapped: *mut WindowsOverlapped,
    ) -> i32;
    fn UnlockFileEx(
        file: *mut std::ffi::c_void,
        reserved: u32,
        bytes_low: u32,
        bytes_high: u32,
        overlapped: *mut WindowsOverlapped,
    ) -> i32;
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
            "vendored {} {} for {} is missing; run `phoxal update` online",
            descriptor.package_id,
            descriptor.version,
            descriptor_scope_label(descriptor)
        );
    }

    if let Some(ui) = ui {
        ui.info(format!(
            "provisioning {} {} from {}",
            descriptor.kind, descriptor.name, descriptor.url
        ));
    }
    let mode = ui.map_or_else(crate::output_mode::OutputMode::from_env, |ui| ui.mode());
    let tarball_path = download_blob(descriptor, mode)?;
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
    let mut package_versions = std::collections::BTreeMap::new();
    for descriptor in descriptors {
        if let Some(existing) = package_versions.insert(&descriptor.package_id, &descriptor.version)
        {
            anyhow::ensure!(
                existing == &descriptor.version,
                "artifact package {} resolved multiple versions in one atomic update: {} and {}",
                descriptor.package_id,
                existing,
                descriptor.version
            );
        }
    }
    let total = descriptors.len() as u64;
    let mut completed = 0_u64;
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
        // Finding C2: real per-batch progress against the "download" phase
        // `prepare_descriptors_with_preflight` brackets - the first genuine
        // producer of `SessionEvent::PhaseProgress` (see
        // `session::diagnostics::phase_progress`'s own docs).
        completed += batch.len() as u64;
        crate::session::diagnostics::phase_progress(
            crate::session::event::PhaseId::new("download"),
            completed,
            total,
        );
        if let Some(ui) = ui {
            for descriptor in batch {
                ui.info(format!(
                    "verified {} {} [{}] ({} bytes)",
                    descriptor.package_id,
                    descriptor.version,
                    descriptor_scope_label(descriptor),
                    descriptor.size
                ));
            }
        }
    }
    for descriptor in descriptors {
        retarget_active(descriptor)?;
    }
    Ok(())
}

/// Return the selected binary through its package-scoped `active` symlink.
pub fn artifact_binary_path(descriptor: &NativeArtifactDescriptor) -> Result<PathBuf> {
    let _lock = ArtifactStoreLock::shared()?;
    let target = descriptor
        .target
        .as_deref()
        .context("component assets do not contain a native binary")?;
    validate_path_segment("artifact target", target)?;
    let version = active_version_unlocked(&descriptor.package_id)?
        .context("vendored artifact package has no active version")?;
    Ok(artifact_package_dir(&descriptor.package_id)?
        .join("versions")
        .join(version)
        .join("targets")
        .join(target)
        .join(&descriptor.binary_name))
}

/// Temporary download path beside the selected version directory.
pub fn artifact_tarball_path(descriptor: &NativeArtifactDescriptor) -> Result<PathBuf> {
    let scope = descriptor.target.as_deref().unwrap_or("assets");
    validate_path_segment("artifact scope", scope)?;
    Ok(artifact_package_dir(&descriptor.package_id)?
        .join("versions")
        .join(format!(".{}-{scope}.partial", descriptor.version)))
}

/// Where `descriptor` is unpacked in the project-local artifact store.
pub fn artifact_exec_dir(descriptor: &NativeArtifactDescriptor) -> Result<PathBuf> {
    validate_path_segment("artifact version", &descriptor.version)?;
    let version = artifact_package_dir(&descriptor.package_id)?
        .join("versions")
        .join(&descriptor.version);
    match descriptor.target.as_deref() {
        Some(target) => {
            validate_path_segment("artifact target", target)?;
            Ok(version.join("targets").join(target))
        }
        None => Ok(version.join("assets")),
    }
}

pub fn artifact_target_dir_for(package: &str, target: &str) -> Result<PathBuf> {
    validate_path_segment("artifact target", target)?;
    Ok(artifact_package_dir(package)?
        .join("active")
        .join("targets")
        .join(target))
}

pub fn artifact_assets_dir_for(package: &str) -> Result<PathBuf> {
    Ok(artifact_package_dir(package)?.join("active").join("assets"))
}

pub fn active_version(descriptor: &NativeArtifactDescriptor) -> Result<Option<String>> {
    active_version_for(&descriptor.package_id)
}

pub fn active_version_for(package: &str) -> Result<Option<String>> {
    let _lock = ArtifactStoreLock::shared()?;
    active_version_unlocked(package)
}

fn active_version_unlocked(package: &str) -> Result<Option<String>> {
    let active = artifact_package_dir(package)?.join("active");
    match fs::read_link(&active) {
        Ok(target) => Ok(target
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("failed to read {}", active.display())),
    }
}

pub fn count_versions() -> Result<usize> {
    let _lock = ArtifactStoreLock::shared()?;
    walk_artifact_versions(false, false, None).map(|(retained, _)| retained)
}

pub fn prune_inactive_versions(current: &[NativeArtifactDescriptor]) -> Result<(usize, usize)> {
    let _lock = ArtifactStoreLock::exclusive("prune")?;
    classify_inactive_versions(current, true)
}

pub fn preview_prune_inactive_versions(
    current: &[NativeArtifactDescriptor],
) -> Result<(usize, usize)> {
    let _lock = ArtifactStoreLock::shared()?;
    classify_inactive_versions(current, false)
}

fn classify_inactive_versions(
    current: &[NativeArtifactDescriptor],
    remove: bool,
) -> Result<(usize, usize)> {
    let packages = current
        .iter()
        .map(|descriptor| descriptor.package_id.clone())
        .collect::<std::collections::BTreeSet<_>>();
    walk_artifact_versions(true, remove, Some(&packages))
}

fn walk_artifact_versions(
    classify_inactive: bool,
    remove: bool,
    current_packages: Option<&std::collections::BTreeSet<String>>,
) -> Result<(usize, usize)> {
    let root = crate::host_paths::artifacts_dir()?;
    if !root.is_dir() {
        return Ok((0, 0));
    }
    let mut retained = 0;
    let mut pruned = 0;
    for provider in fs::read_dir(&root)? {
        let provider = provider?;
        if !provider.file_type()?.is_dir() {
            continue;
        }
        for package in fs::read_dir(provider.path())? {
            let package = package?;
            if !package.file_type()?.is_dir() {
                continue;
            }
            let package_id = format!(
                "{}/{}",
                provider.file_name().to_string_lossy(),
                package.file_name().to_string_lossy()
            );
            let versions = package.path().join("versions");
            if !versions.is_dir() {
                continue;
            }
            let keep_package =
                current_packages.is_none_or(|packages| packages.contains(&package_id));
            if classify_inactive && !keep_package {
                pruned += fs::read_dir(&versions)?
                    .filter_map(Result::ok)
                    .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
                    .count();
                if remove {
                    fs::remove_dir_all(package.path())?;
                }
                continue;
            }
            let active = fs::read_link(package.path().join("active"))
                .ok()
                .and_then(|path| path.file_name().map(|name| name.to_os_string()));
            for version in fs::read_dir(&versions)? {
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
    let targets_dir = artifact_package_dir(package)?
        .join("active")
        .join("targets");
    if !targets_dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut targets = Vec::new();
    for entry in fs::read_dir(targets_dir)? {
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

fn download_blob(
    descriptor: &NativeArtifactDescriptor,
    mode: crate::output_mode::OutputMode,
) -> Result<PathBuf> {
    let label = format!(
        "downloading {} {} [{}] ({} bytes)",
        descriptor.package_id,
        descriptor.version,
        descriptor_scope_label(descriptor),
        descriptor.size
    );
    // `descriptor.size` is the catalog-declared blob size (always known
    // ahead of the request - it is what `verify_blob_bytes` checks the
    // download against), so the byte bar is always determinate here.
    let progress = crate::progress::bytes_bar(label, descriptor.size, mode);
    match download_blob_inner(descriptor, &progress) {
        Ok(path) => {
            progress.finish_and_clear();
            Ok(path)
        }
        Err(error) => {
            progress.abandon_with_message(format!(
                "failed to download {} {}: {error:#}",
                descriptor.package_id, descriptor.version
            ));
            Err(error)
        }
    }
}

fn download_blob_inner(
    descriptor: &NativeArtifactDescriptor,
    progress: &crate::progress::Handle,
) -> Result<PathBuf> {
    use std::io::Read;

    let url = &descriptor.url;
    let client = reqwest::blocking::Client::builder()
        .user_agent("phoxal-cli")
        .timeout(Duration::from_secs(120))
        .build()?;
    let mut response = client
        .get(url)
        .send()
        .with_context(|| format!("failed to download {url}"))?;
    if !response.status().is_success() {
        bail!("download from {url} returned {}", response.status());
    }
    let mut bytes = Vec::with_capacity(descriptor.size as usize);
    let mut chunk = [0_u8; 64 * 1024];
    loop {
        let read = response
            .read(&mut chunk)
            .with_context(|| format!("failed to read {url} body"))?;
        if read == 0 {
            break;
        }
        bytes.extend_from_slice(&chunk[..read]);
        progress.inc(read as u64);
    }
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
    let package_dir = artifact_package_dir(&descriptor.package_id)?;
    fs::create_dir_all(package_dir.join("versions"))?;
    let partial = package_dir.join(".active.partial");
    if fs::symlink_metadata(&partial).is_ok() {
        fs::remove_file(&partial)?;
    }
    #[cfg(unix)]
    std::os::unix::fs::symlink(Path::new("versions").join(&descriptor.version), &partial)?;
    #[cfg(windows)]
    std::os::windows::fs::symlink_dir(Path::new("versions").join(&descriptor.version), &partial)?;
    fs::rename(&partial, package_dir.join("active"))
        .context("failed to atomically retarget the active artifact version")
}

fn unpack_asset(asset_path: &Path, root: &Path) -> Result<()> {
    let partial = root
        .parent()
        .context("artifact scope has no parent")?
        .join(format!(
            ".{}.partial",
            root.file_name()
                .context("artifact scope has no directory name")?
                .to_string_lossy()
        ));
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

fn package_storage_key(package: &str) -> Result<(String, String)> {
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

fn artifact_package_dir(package: &str) -> Result<PathBuf> {
    let (provider, package) = package_storage_key(package)?;
    Ok(crate::host_paths::artifacts_dir()?
        .join(provider)
        .join(package))
}

fn descriptor_scope_label(descriptor: &NativeArtifactDescriptor) -> &str {
    descriptor.target.as_deref().unwrap_or("assets")
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
            target: Some("aarch64-unknown-linux-musl".to_string()),
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
    fn local_identity_is_validated_and_filesystem_safe() -> Result<()> {
        // Matches `filesystem_safe_package_name` used everywhere else in the
        // system, so a package maps to the same on-disk name in the store, the
        // resolver, the deploy install plan, and the framework's release tags.
        assert_eq!(
            package_storage_key("phoxal/service-drive")?,
            ("phoxal".to_string(), "service-drive".to_string())
        );
        assert!(package_storage_key("../service-drive").is_err());
        assert!(package_storage_key("phoxal/service/drive").is_err());

        let mut invalid = descriptor("../escape", b"bytes");
        assert!(artifact_exec_dir(&invalid).is_err());
        invalid.version = "1.0.0".to_string();
        invalid.target = Some("../../escape".to_string());
        assert!(artifact_exec_dir(&invalid).is_err());
        Ok(())
    }

    #[test]
    fn layout_is_provider_scoped_and_version_atomic() -> Result<()> {
        let _root = ScratchPhoxalHome::new()?;
        let target = descriptor("1.2.3", b"target");
        let mut assets = target.clone();
        assets.kind = ArtifactKind::ComponentAssets;
        assets.binary_name.clear();
        assets.target = None;

        assert!(artifact_exec_dir(&target)?.ends_with(
            "artifacts/phoxal/service-drive/versions/1.2.3/targets/aarch64-unknown-linux-musl"
        ));
        assert!(
            artifact_exec_dir(&assets)?
                .ends_with("artifacts/phoxal/service-drive/versions/1.2.3/assets")
        );
        Ok(())
    }

    #[test]
    fn lock_file_self_heals_and_is_removed_after_exclusive_work() -> Result<()> {
        let _root = ScratchPhoxalHome::new()?;
        let lock_path = crate::host_paths::artifacts_dir()?.join(".lock");
        fs::create_dir_all(lock_path.parent().context("lock has no parent")?)?;
        fs::write(&lock_path, b"stale")?;

        let lock = ArtifactStoreLock::exclusive("test")?;
        assert!(lock_path.is_file());
        drop(lock);

        assert!(!lock_path.exists());
        Ok(())
    }

    #[test]
    fn last_shared_holder_removes_lock_file() -> Result<()> {
        let _root = ScratchPhoxalHome::new()?;
        let lock_path = crate::host_paths::artifacts_dir()?.join(".lock");
        let first = ArtifactStoreLock::shared()?;
        let second = ArtifactStoreLock::shared()?;

        drop(first);
        assert!(lock_path.is_file());
        drop(second);

        assert!(!lock_path.exists());
        Ok(())
    }

    /// Finding A3: a warm cache (every actionable descriptor already staged)
    /// must emit NO `download` phase - Product decision 3 forbids showing a
    /// phase for work that never runs. Uses a descriptor whose exec dir is
    /// pre-created so `prepare_descriptor` takes its `MissingOnly` early
    /// return and never reaches the network, regardless of the (unreachable)
    /// URL.
    #[tokio::test]
    async fn prepare_descriptors_with_preflight_emits_no_download_phase_on_a_warm_cache()
    -> Result<()> {
        let _diagnostics_guard = crate::session::diagnostics::DIAGNOSTICS_TEST_LOCK
            .lock()
            .await;
        let _root = ScratchPhoxalHome::new()?;
        let mut staged = descriptor("1.0.0", b"already-staged");
        staged.url = "http://127.0.0.1:1/drive.tar".to_string();
        fs::create_dir_all(artifact_exec_dir(&staged)?)?;

        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        crate::session::diagnostics::install(tx);

        prepare_descriptors_with_preflight(std::slice::from_ref(&staged), None)?;

        crate::session::diagnostics::uninstall();
        while let Ok(event) = rx.try_recv() {
            assert!(
                !matches!(
                    event,
                    crate::session::event::SessionEvent::PhaseStarted { .. }
                        | crate::session::event::SessionEvent::PhaseFinished { .. }
                ),
                "a warm cache must not emit a download phase, got {event:?}"
            );
        }
        Ok(())
    }

    /// The fresh-cache counterpart: a descriptor with no staged exec dir must
    /// genuinely attempt a download, so a `download` phase must appear -
    /// started AND finished, even though the download itself fails (an
    /// unroutable localhost port stands in for "no network available",
    /// keeping this test fast and deterministic without a real artifact
    /// server).
    #[tokio::test]
    async fn prepare_descriptors_with_preflight_emits_a_download_phase_on_a_cold_cache()
    -> Result<()> {
        let _diagnostics_guard = crate::session::diagnostics::DIAGNOSTICS_TEST_LOCK
            .lock()
            .await;
        let _root = ScratchPhoxalHome::new()?;
        let mut cold = descriptor("1.0.0", b"never-staged");
        cold.url = "http://127.0.0.1:1/drive.tar".to_string();

        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        crate::session::diagnostics::install(tx);

        let result = prepare_descriptors_with_preflight(std::slice::from_ref(&cold), None);
        crate::session::diagnostics::uninstall();
        assert!(result.is_err(), "an unroutable download must fail");

        let mut saw_started = false;
        let mut saw_finished_failed = false;
        while let Ok(event) = rx.try_recv() {
            match event {
                crate::session::event::SessionEvent::PhaseStarted { id, .. }
                    if id.as_str() == "download" =>
                {
                    saw_started = true;
                }
                crate::session::event::SessionEvent::PhaseFinished { id, outcome, .. }
                    if id.as_str() == "download" =>
                {
                    assert!(
                        matches!(outcome, crate::session::event::PhaseOutcome::Failed { .. }),
                        "the download phase must report its real failure, got {outcome:?}"
                    );
                    saw_finished_failed = true;
                }
                _ => {}
            }
        }
        assert!(saw_started, "a cold cache must start a download phase");
        assert!(
            saw_finished_failed,
            "a cold cache's failed download must still finish its phase"
        );
        Ok(())
    }

    /// A minimal local HTTP/1.1 server that serves `body` for exactly one
    /// request, then exits - just enough for a real `reqwest::blocking`
    /// download to succeed without reaching any external network. The
    /// returned `JoinHandle` is intentionally left unjoined: the server
    /// thread exits on its own once it has served its one request.
    fn spawn_minimal_http_server(body: Vec<u8>) -> std::net::SocketAddr {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind local http server");
        let addr = listener.local_addr().expect("local server address");
        std::thread::spawn(move || {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let mut buf = [0_u8; 1024];
            let _ = stream.read(&mut buf);
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.write_all(&body);
            let _ = stream.flush();
        });
        addr
    }

    /// Build a real, minimal `.tar.gz` archive containing one flat file named
    /// `entry_name` - enough for `unpack_asset`'s real `tar -xf` (via the
    /// system `tar` binary) to succeed for real, unlike a fake byte blob.
    fn minimal_tar_gz(entry_name: &str, contents: &[u8]) -> Result<Vec<u8>> {
        let mut tar_bytes = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut tar_bytes);
            let mut header = tar::Header::new_gnu();
            header.set_size(contents.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            builder.append_data(&mut header, entry_name, contents)?;
            builder.finish()?;
        }
        let mut gz_bytes = Vec::new();
        {
            let mut encoder =
                flate2::write::GzEncoder::new(&mut gz_bytes, flate2::Compression::fast());
            std::io::Write::write_all(&mut encoder, &tar_bytes)?;
            encoder.finish()?;
        }
        Ok(gz_bytes)
    }

    /// Finding C2: `PhaseProgress` used to be constructed only by a render
    /// test, never by production code. This exercises the REAL download
    /// pipeline end to end (a genuine HTTP download of a real tar.gz archive,
    /// unpacked by the system `tar`) and asserts a real
    /// `SessionEvent::PhaseProgress` for the "download" phase comes out the
    /// other end, not just `PhaseStarted`/`PhaseFinished`.
    #[tokio::test]
    async fn prepare_descriptors_with_preflight_emits_real_download_progress_on_success()
    -> Result<()> {
        let _diagnostics_guard = crate::session::diagnostics::DIAGNOSTICS_TEST_LOCK
            .lock()
            .await;
        let _root = ScratchPhoxalHome::new()?;

        let archive_bytes = minimal_tar_gz("phoxal-service-drive", b"#!/bin/sh\n")?;
        let addr = spawn_minimal_http_server(archive_bytes.clone());
        let mut fresh = descriptor("1.0.0", &archive_bytes);
        fresh.url = format!("http://{addr}/drive.tar.gz");

        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        crate::session::diagnostics::install(tx);
        let result = prepare_descriptors_with_preflight(std::slice::from_ref(&fresh), None);
        crate::session::diagnostics::uninstall();
        result?;

        let mut saw_progress = false;
        while let Ok(event) = rx.try_recv() {
            if let crate::session::event::SessionEvent::PhaseProgress {
                id,
                completed,
                total,
                ..
            } = event
            {
                assert_eq!(id.as_str(), "download");
                assert_eq!(completed, 1);
                assert_eq!(total, 1);
                saw_progress = true;
            }
        }
        assert!(
            saw_progress,
            "a real successful download must emit real PhaseProgress, not just Started/Finished"
        );
        Ok(())
    }
}
