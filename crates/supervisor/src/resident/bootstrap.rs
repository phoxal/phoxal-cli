use std::fs::File;
use std::os::fd::FromRawFd;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Result;
use phoxal_cli_core::identity::ExecutionId;
use phoxal_cli_protocol::BootstrapResult;
use phoxal_cli_protocol::codec::blocking_io;
use phoxal_cli_protocol::limits::MAX_HANDSHAKE_FRAME_BYTES;

pub(super) const BOOTSTRAP_FD_ENV: &str = "PHOXAL_RESIDENT_BOOTSTRAP_FD";
pub(super) const BOOTSTRAP_EXECUTION_ENV: &str = "PHOXAL_RESIDENT_EXECUTION_ID";
static BOOTSTRAP_REPORTED: AtomicBool = AtomicBool::new(false);

/// The supervised run this resident was launched to adopt, if it was launched
/// privately.
pub fn private_bootstrap_execution() -> Result<Option<ExecutionId>> {
    let Some(value) = std::env::var_os(BOOTSTRAP_EXECUTION_ENV) else {
        return Ok(None);
    };
    ExecutionId::parse(&value.to_string_lossy())
        .map(Some)
        .map_err(|error| anyhow::anyhow!("invalid private resident execution id: {error}"))
}

pub fn report_private_bootstrap(result: &BootstrapResult) -> Result<()> {
    if BOOTSTRAP_REPORTED.swap(true, Ordering::SeqCst) {
        return Ok(());
    }
    let Some(fd) = std::env::var(BOOTSTRAP_FD_ENV)
        .ok()
        .and_then(|value| value.parse::<i32>().ok())
    else {
        return Ok(());
    };
    // SAFETY: this descriptor was supplied by our launcher for this one-shot
    // bootstrap and ownership transfers to this File exactly once.
    let mut socket = unsafe { File::from_raw_fd(fd) };
    blocking_io::write_frame(&mut socket, result, MAX_HANDSHAKE_FRAME_BYTES)
}
