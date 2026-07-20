//! Environment responsibilities for run.

use crate::resolver::host_target_triple;
use phoxal_cli_core::project::resolver::ResolvedPlatformRuntime;
use std::path::PathBuf;

pub(crate) fn env_path_override(prefix: &str, id: &str) -> Option<PathBuf> {
    let key = format!("{prefix}_{}_PATH", env_key(id));
    std::env::var_os(key)
        .map(PathBuf::from)
        .filter(|path| path.is_file())
}

pub(crate) fn env_key(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect()
}

pub(crate) fn native_pending_tool_note(name: &str) -> String {
    format!(
        "NativePending: {name} binary is not in the artifact cache; set PHOXAL_ARTIFACT_{}_PATH, PHOXAL_ARTIFACT_DIR, PHOXAL_TOOL_{}_PATH, or PHOXAL_TOOL_DIR",
        env_key(name),
        env_key(name)
    )
}

pub(crate) fn native_pending_official_note(
    runtime: Option<&ResolvedPlatformRuntime>,
    participant_id: &str,
) -> String {
    let status = match runtime {
        Some(runtime) if runtime.published => "released",
        _ => "missing",
    };
    let target = host_target_triple();
    format!(
        "NativePending: official artifact {participant_id} is {status} for {target} or not vendored; run `phoxal update`, set PHOXAL_ARTIFACT_{}_PATH, or set PHOXAL_ARTIFACT_DIR",
        env_key(participant_id)
    )
}
