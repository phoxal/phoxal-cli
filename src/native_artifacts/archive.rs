//! Archive extraction into immutable digest scopes.

use super::SCOPE_DIGEST_FILE;
use crate::ui::Ui;
use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use flate2::read::GzDecoder;
use std::fs;
use std::path::Path;
use std::process::Command;
use tar::Archive;

pub(crate) fn unpack_asset(asset_path: &Path, root: &Path, digest: &str) -> Result<()> {
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

    unpack_archive(asset_path, &partial)?;

    fs::write(partial.join(SCOPE_DIGEST_FILE), format!("{digest}\n"))
        .context("failed to record the verified artifact digest")?;

    if root.exists() {
        fs::remove_dir_all(root)
            .with_context(|| format!("failed to replace {}", root.display()))?;
    }
    fs::rename(&partial, root).with_context(|| format!("failed to finalize {}", root.display()))
}

pub(crate) fn unpack_archive(asset_path: &Path, dest: &Path) -> Result<()> {
    if asset_path
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.ends_with(".tar.gz"))
    {
        unpack_tar_gz(asset_path, dest)
    } else if asset_path
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.ends_with(".tar"))
    {
        unpack_tar(asset_path, dest)
    } else {
        unpack_with_system_tar(asset_path, dest)
    }
}

pub(crate) fn unpack_tar_gz(asset_path: &Path, dest: &Path) -> Result<()> {
    let file = fs::File::open(asset_path)
        .with_context(|| format!("failed to open {}", asset_path.display()))?;
    let mut archive = Archive::new(GzDecoder::new(file));
    archive
        .unpack(dest)
        .with_context(|| format!("failed to unpack {}", asset_path.display()))
}

pub(crate) fn unpack_tar(asset_path: &Path, dest: &Path) -> Result<()> {
    let file = fs::File::open(asset_path)
        .with_context(|| format!("failed to open {}", asset_path.display()))?;
    let mut archive = Archive::new(file);
    archive
        .unpack(dest)
        .with_context(|| format!("failed to unpack {}", asset_path.display()))
}

pub(crate) fn unpack_with_system_tar(asset_path: &Path, dest: &Path) -> Result<()> {
    let mut command = Command::new("tar");
    command.arg("-xf").arg(asset_path).arg("-C").arg(dest);
    // Keep the uncommon system-tar fallback inside the same captured-output
    // and cancellation path as cargo builds. In particular, tar diagnostics
    // must not draw underneath an active TUI or leak onto JSON stderr.
    let status = Ui::from_env()
        .command_status_captured(&mut command)
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
