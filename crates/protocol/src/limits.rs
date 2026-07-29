//! Protocol frame and bounded-state limits.

use std::time::Duration;

pub const FRAME_READ_TIMEOUT: Duration = Duration::from_secs(5);
pub const FRAME_WRITE_TIMEOUT: Duration = Duration::from_secs(5);
pub const MAX_SUPERVISED_PROCESSES: usize = phoxal_cli_core::runtime::MAX_SUPERVISED_PROCESSES;
pub const MAX_PROCESS_FAILURE_DETAIL_BYTES: usize =
    phoxal_cli_core::runtime::BoundedString::FAILURE_MAX_BYTES;
pub const MAX_PROCESS_STDERR_TAIL_BYTES: usize = phoxal_cli_core::runtime::BoundedString::MAX_BYTES;
pub const MAX_ARTIFACT_ID_BYTES: usize = phoxal_cli_core::runtime::MAX_RUNTIME_ARTIFACT_ID_BYTES;
pub const MAX_SNAPSHOT_TEXT_BYTES: usize = phoxal_cli_core::runtime::MAX_RUNTIME_TEXT_BYTES;
pub const MAX_HANDSHAKE_FRAME_BYTES: usize = 4 * 1024;
pub const MAX_COMMAND_FRAME_BYTES: usize = 64 * 1024;
pub const MAX_SNAPSHOT_FRAME_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_RECENT_COMMAND_REPLIES: usize = 128;
pub const MAX_STARTUP_PHASES: usize = 64;

/// Conservative encoded upper bound for one maximum-size process entry.
///
/// JSON escaping can expand every byte to six bytes (`\u00xx`), so the
/// calculation deliberately assumes that worst case for every bounded text
/// field and includes structural headroom.
pub const MAX_ENCODED_PROCESS_BYTES: usize = 6
    * (3 * MAX_ARTIFACT_ID_BYTES
        + MAX_PROCESS_FAILURE_DETAIL_BYTES
        + MAX_PROCESS_STDERR_TAIL_BYTES
        + 5 * MAX_SNAPSHOT_TEXT_BYTES)
    + 8 * 1024;
pub const MAX_ENCODED_SNAPSHOT_FIXED_BYTES: usize =
    6 * (10 + MAX_STARTUP_PHASES + 1) * MAX_SNAPSHOT_TEXT_BYTES + 64 * 1024;

#[must_use]
pub const fn worst_case_snapshot_bytes(process_count: usize) -> Option<usize> {
    match MAX_ENCODED_PROCESS_BYTES.checked_mul(process_count) {
        Some(processes) => processes.checked_add(MAX_ENCODED_SNAPSHOT_FIXED_BYTES),
        None => None,
    }
}

pub fn validate_snapshot_capacity(process_count: usize) -> anyhow::Result<()> {
    anyhow::ensure!(
        process_count <= MAX_SUPERVISED_PROCESSES,
        "execution plan has {process_count} supervised processes; protocol v0 supports at most {MAX_SUPERVISED_PROCESSES}"
    );
    let bytes = worst_case_snapshot_bytes(process_count)
        .ok_or_else(|| anyhow::anyhow!("worst-case supervisor snapshot size overflow"))?;
    anyhow::ensure!(
        bytes <= MAX_SNAPSHOT_FRAME_BYTES,
        "execution plan worst-case supervisor snapshot is {bytes} bytes; protocol v0 frame ceiling is {MAX_SNAPSHOT_FRAME_BYTES} bytes"
    );
    Ok(())
}
