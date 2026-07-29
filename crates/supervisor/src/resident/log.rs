use std::fs::{File, OpenOptions};
use std::path::PathBuf;

use anyhow::{Context, Result};

const RESIDENT_LOG_MAX_BYTES: u64 = 2 * 1024 * 1024;
const RESIDENT_LOG_ROTATIONS: usize = 3;

/// Open the supervisor log for appending, returning the handle together with
/// its path so launch errors can point the operator at the file.
pub(super) fn resident_log_file() -> Result<(File, PathBuf)> {
    let project = std::env::var_os(phoxal_cli_core::runtime::PROJECT_ROOT_ENV)
        .map(PathBuf::from)
        .unwrap_or(std::env::current_dir()?);
    let path = phoxal_cli_core::runtime::paths::RuntimePaths::for_root(&project).supervisor_log();
    let directory = path
        .parent()
        .expect("supervisor log has a parent")
        .to_path_buf();
    std::fs::create_dir_all(&directory)?;
    if path
        .metadata()
        .is_ok_and(|metadata| metadata.len() >= RESIDENT_LOG_MAX_BYTES)
    {
        for index in (1..=RESIDENT_LOG_ROTATIONS).rev() {
            let source = if index == 1 {
                path.clone()
            } else {
                directory.join(format!("supervisor.log.{}", index - 1))
            };
            let destination = directory.join(format!("supervisor.log.{index}"));
            if source.exists() {
                let _ = std::fs::rename(source, destination);
            }
        }
    }
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("open resident log {}", path.display()))?;
    Ok((file, path))
}
