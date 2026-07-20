//! Single-descriptor staging entry point.

use super::{ArtifactStoreLock, prepare_descriptor, retarget_active};
use crate::ui::Ui;
use anyhow::Result;
use phoxal_cli_core::artifacts::NativeArtifactDescriptor;
use phoxal_cli_core::artifacts::ProvisioningMode;
use std::path::PathBuf;

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
