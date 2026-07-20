//! Artifact download and digest verification.

use super::{artifact_tarball_path, descriptor_scope_label, write_file_atomic};
use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use phoxal_cli_core::artifacts::NativeArtifactDescriptor;
use phoxal_cli_core::session::human;
use sha2::Digest;
use sha2::Sha256;
use std::path::PathBuf;
use std::time::Duration;

pub(crate) fn download_blob(descriptor: &NativeArtifactDescriptor) -> Result<PathBuf> {
    let label = format!(
        "downloading {} {} [{}] ({})",
        descriptor.package_id,
        descriptor.version,
        descriptor_scope_label(descriptor),
        human::bytes(descriptor.size)
    );
    let progress = crate::progress::status(label);
    match download_blob_inner(descriptor) {
        Ok(path) => Ok(path),
        Err(error) => {
            progress.abandon_with_message(format!(
                "failed to download {} {}: {error:#}",
                descriptor.package_id, descriptor.version
            ));
            Err(error)
        }
    }
}

pub(crate) fn download_blob_inner(descriptor: &NativeArtifactDescriptor) -> Result<PathBuf> {
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
    }
    verify_blob_bytes(descriptor, &bytes)?;
    let path = artifact_tarball_path(descriptor)?;
    write_file_atomic(&path, &bytes)?;
    Ok(path)
}

pub(crate) fn verify_blob_bytes(descriptor: &NativeArtifactDescriptor, bytes: &[u8]) -> Result<()> {
    if bytes.len() as u64 != descriptor.size {
        bail!(
            "size mismatch for {} {}: expected {}, got {}",
            descriptor.package_id,
            descriptor.version,
            human::bytes(descriptor.size),
            human::bytes(bytes.len() as u64)
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
