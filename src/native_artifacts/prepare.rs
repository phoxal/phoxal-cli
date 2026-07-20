//! Descriptor download, unpack, and prepared-scope construction.

use super::{
    ArtifactProgressReporter, SCOPE_DIGEST_FILE, artifact_exec_dir, descriptor_is_current,
    descriptor_scope_label, download_blob, download_blob_inner, unpack_archive, unpack_asset,
};
use crate::ui::Ui;
use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use phoxal_cli_core::artifacts::NativeArtifactDescriptor;
use phoxal_cli_core::artifacts::ProvisioningMode;
use phoxal_cli_core::project::tooling::make_executable;
use std::fs;
use std::path::PathBuf;

pub(crate) fn prepare_descriptor(
    ui: Option<&Ui>,
    descriptor: &NativeArtifactDescriptor,
    mode: ProvisioningMode,
) -> Result<PathBuf> {
    prepare_descriptor_inner(ui, descriptor, mode, None)
}

pub(crate) fn prepare_descriptor_inner(
    ui: Option<&Ui>,
    descriptor: &NativeArtifactDescriptor,
    mode: ProvisioningMode,
    reporter: Option<&dyn ArtifactProgressReporter>,
) -> Result<PathBuf> {
    let version_dir = artifact_exec_dir(descriptor)?;
    let binary = version_dir.join(&descriptor.binary_name);
    if mode == ProvisioningMode::MissingOnly && descriptor_is_current(descriptor) {
        if let Some(reporter) = reporter {
            reporter.complete(descriptor, None);
        }
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

    if reporter.is_none()
        && let Some(ui) = ui
    {
        ui.info(format!(
            "provisioning {} {} from {}",
            descriptor.kind, descriptor.name, descriptor.url
        ));
    }
    let row = reporter.map(|reporter| reporter.begin(descriptor));
    let result = (|| {
        let tarball_path = if row.is_some() {
            download_blob_inner(descriptor)?
        } else {
            download_blob(descriptor)?
        };
        unpack_asset(&tarball_path, &version_dir, &descriptor.sha256)?;
        fs::remove_file(&tarball_path).ok();
        if binary.is_file() {
            make_executable(&binary)?;
        }
        Ok(binary)
    })();
    match result {
        Ok(binary) => {
            if let Some(reporter) = reporter {
                reporter.complete(descriptor, row);
            }
            Ok(binary)
        }
        Err(error) => {
            if let (Some(reporter), Some(row)) = (reporter, row) {
                reporter.failed(descriptor, row, &error);
            }
            Err(error)
        }
    }
}

pub(crate) struct PreparedScope {
    pub(crate) final_root: PathBuf,
    pub(crate) candidate_root: Option<PathBuf>,
}

impl Drop for PreparedScope {
    fn drop(&mut self) {
        if let Some(candidate) = self.candidate_root.take() {
            let _ = fs::remove_dir_all(candidate);
        }
    }
}

pub(crate) fn prepare_scope_candidate(
    descriptor: &NativeArtifactDescriptor,
    ui: Option<&Ui>,
    reporter: Option<&dyn ArtifactProgressReporter>,
) -> Result<Option<PreparedScope>> {
    if descriptor_is_current(descriptor) {
        if let Some(reporter) = reporter {
            reporter.complete(descriptor, None);
        }
        return Ok(None);
    }
    if descriptor.url.is_empty() {
        bail!(
            "vendored {} {} for {} is missing; run `phoxal update` online",
            descriptor.package_id,
            descriptor.version,
            descriptor_scope_label(descriptor)
        );
    }
    if reporter.is_none()
        && let Some(ui) = ui
    {
        ui.info(format!(
            "provisioning {} {} from {}",
            descriptor.kind, descriptor.name, descriptor.url
        ));
    }
    let row = reporter.map(|reporter| reporter.begin(descriptor));
    let result = (|| {
        let tarball_path = if row.is_some() {
            download_blob_inner(descriptor)?
        } else {
            download_blob(descriptor)?
        };
        let final_root = artifact_exec_dir(descriptor)?;
        let parent = final_root
            .parent()
            .context("artifact scope has no parent")?;
        fs::create_dir_all(parent)?;
        let candidate = tempfile::Builder::new()
            .prefix(".scope-candidate-")
            .tempdir_in(parent)?;
        unpack_archive(&tarball_path, candidate.path())?;
        fs::write(
            candidate.path().join(SCOPE_DIGEST_FILE),
            format!("{}\n", descriptor.sha256),
        )?;
        fs::remove_file(&tarball_path).ok();
        let binary = candidate.path().join(&descriptor.binary_name);
        if binary.is_file() {
            make_executable(&binary)?;
        }
        Ok(PreparedScope {
            final_root,
            candidate_root: Some(candidate.keep()),
        })
    })();
    match result {
        Ok(prepared) => {
            if let Some(reporter) = reporter {
                reporter.complete(descriptor, row);
            }
            Ok(Some(prepared))
        }
        Err(error) => {
            if let (Some(reporter), Some(row)) = (reporter, row) {
                reporter.failed(descriptor, row, &error);
            }
            Err(error)
        }
    }
}
