//! Descriptor selection, capacity preflight, and progress reporting.

use super::{
    ArtifactStoreLock, descriptor_is_current, prepare_and_activate_descriptors, stage_descriptor,
};
use crate::ui::Ui;
use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use phoxal_cli_core::artifacts::NativeArtifactDescriptor;
use phoxal_cli_core::artifacts::ProvisioningMode;
use phoxal_cli_core::project::resolver::ResolvedPlatformRuntime;
use phoxal_cli_core::project::resolver::ResolvedTool;
use phoxal_cli_core::session::human;
use std::path::Path;
use std::path::PathBuf;

/// Presentation hook for commands that own a multi-row artifact view.
pub(crate) trait ArtifactProgressReporter: Sync {
    fn begin(&self, descriptor: &NativeArtifactDescriptor) -> crate::progress::Row;
    fn complete(&self, descriptor: &NativeArtifactDescriptor, row: Option<crate::progress::Row>);
    fn failed(
        &self,
        descriptor: &NativeArtifactDescriptor,
        row: crate::progress::Row,
        error: &anyhow::Error,
    );
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

pub fn descriptors(
    resolved: &phoxal_cli_core::project::resolver::ResolvedRobot,
) -> Result<Vec<NativeArtifactDescriptor>> {
    phoxal_cli_core::artifacts::descriptors(resolved)
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
    let stale = actionable
        .iter()
        .filter(|descriptor| !descriptor_is_current(descriptor))
        .collect::<Vec<_>>();
    if !stale.is_empty() {
        let total_bytes = stale.iter().map(|descriptor| descriptor.size).sum::<u64>();
        let destination = crate::host_paths::artifacts_dir()?;
        let free = free_disk_bytes(&destination).ok();
        if let Some(ui) = ui {
            ui.info(format!(
                "artifact preflight: {} package target(s), {}, destination {}",
                stale.len(),
                human::bytes(total_bytes),
                destination.display()
            ));
            if let Some(free) = free {
                ui.info(format!("free disk: {}", human::bytes(free)));
            }
        }
        if let Some(free) = free
            && total_bytes > free
        {
            bail!(
                "artifact download needs {} but only {} are free at {}; stop active Phoxal commands and remove the project-local `.phoxal/` directory, or free disk space",
                human::bytes(total_bytes),
                human::bytes(free),
                destination.display()
            );
        }
    }
    if actionable.is_empty() {
        return Ok(());
    }
    let _lock = ArtifactStoreLock::exclusive("provision")?;
    if stale.is_empty() {
        // Finding A3: every actionable descriptor is already staged (a warm
        // cache) - only cheap activation/retargeting remains, which is not
        // itself download work, so no "download" phase appears (Product
        // decision 3).
        return prepare_and_activate_descriptors(&actionable, ui);
    }
    let count = stale.len();
    crate::session::diagnostics::run_phase(
        phoxal_cli_core::session::event::PhaseId::new("download"),
        format!(
            "Downloading {count} artifact package{}",
            if count == 1 { "" } else { "s" }
        ),
        || prepare_and_activate_descriptors(&actionable, ui),
    )
}

/// Strictly validate that every artifact `descriptors` requires is already
/// vendored and digest-current in the project-local `.phoxal/artifacts` store,
/// without touching the network (#936, finding 1). This replaces the download
/// preflight on the `run`/`start`/`build`/`watch` execution paths: those paths
/// never fetch, so a missing or stale artifact is a hard, actionable error that
/// names what is absent and points at `phoxal update`, rather than a silent
/// download. Descriptors with no URL (source overrides, asset-only entries) are
/// resolved elsewhere and are not part of the vendored-blob completeness set.
pub(crate) fn ensure_vendored_completeness(descriptors: &[NativeArtifactDescriptor]) -> Result<()> {
    let missing = descriptors
        .iter()
        .filter(|descriptor| should_prepare_descriptor(descriptor))
        .filter(|descriptor| !descriptor_is_current(descriptor))
        .map(|descriptor| {
            format!(
                "{} [{}] {}",
                descriptor.package_id,
                descriptor.target.as_deref().unwrap_or("assets"),
                descriptor.version
            )
        })
        .collect::<Vec<_>>();
    if missing.is_empty() {
        return Ok(());
    }
    bail!(
        "the locked train's artifacts are not fully vendored for this project; run `phoxal update` to fetch them. Missing or stale: {}",
        missing.join(", ")
    );
}

pub(crate) fn should_prepare_descriptor(descriptor: &NativeArtifactDescriptor) -> bool {
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
pub(crate) fn free_disk_bytes(path: &Path) -> Result<u64> {
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
pub(crate) fn free_disk_bytes(_path: &Path) -> Result<u64> {
    bail!("free-disk reporting is unavailable on this platform")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host_paths::test_support::ScratchPhoxalHome;
    use phoxal_cli_core::project::suite::ArtifactKind;

    fn descriptor(url: &str, sha: &str) -> NativeArtifactDescriptor {
        NativeArtifactDescriptor {
            package_id: "phoxal/service-drive".to_string(),
            kind: ArtifactKind::Service,
            name: "drive".to_string(),
            version: "0.1.0".to_string(),
            url: url.to_string(),
            sha256: sha.to_string(),
            size: 0,
            binary_name: "phoxal-service-drive".to_string(),
            target: Some(crate::resolver::host_target_triple()),
        }
    }

    /// A required artifact that is not vendored fails the completeness check with
    /// the actionable "run `phoxal update`" error - and, because the check is
    /// filesystem-only, it never attempts a fetch: the URL points at an
    /// unroutable address, so any connection attempt would hang/time out rather
    /// than return the update error immediately (#936, finding 1).
    #[test]
    fn cold_store_completeness_points_at_phoxal_update_without_network() {
        let _scratch = ScratchPhoxalHome::new().unwrap();
        let cold = descriptor("https://10.255.255.1/drive.tar.gz", &"a".repeat(64));
        let error = ensure_vendored_completeness(std::slice::from_ref(&cold))
            .expect_err("a cold store must fail completeness");
        let message = format!("{error:#}");
        assert!(message.contains("phoxal update"), "{message}");
        assert!(message.contains("phoxal/service-drive"), "{message}");
    }

    /// Descriptors with no vendored blob (a source override or asset-only entry)
    /// are resolved elsewhere and never block completeness.
    #[test]
    fn source_override_descriptors_are_not_required_to_be_vendored() {
        let _scratch = ScratchPhoxalHome::new().unwrap();
        let no_url = descriptor("", "");
        ensure_vendored_completeness(std::slice::from_ref(&no_url))
            .expect("no-url descriptors are not part of the vendored set");
    }
}
