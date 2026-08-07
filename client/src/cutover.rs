//! The one place the split's deletions are asserted rather than described.
//!
//! Every symbol below named a piece of the private resident IPC, the
//! in-process daemon, or the supervisor-generation identity axis. They are not
//! deprecated, shimmed, or feature-gated: they are gone, and this crate must
//! not grow them back.

#![cfg(test)]

use std::path::Path;

/// Every Rust source file in this binary crate.
fn sources() -> Vec<(std::path::PathBuf, String)> {
    fn walk(directory: &Path, into: &mut Vec<(std::path::PathBuf, String)>) {
        let entries = std::fs::read_dir(directory).expect("the crate source tree is readable");
        for entry in entries {
            let path = entry.expect("a readable directory entry").path();
            if path.is_dir() {
                walk(&path, into);
            } else if path.extension().is_some_and(|extension| extension == "rs") {
                let text = std::fs::read_to_string(&path).expect("a readable source file");
                into.push((path, text));
            }
        }
    }
    let mut sources = Vec::new();
    walk(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("src").as_path(),
        &mut sources,
    );
    // This file is the one place the deleted names are written down, so it
    // must not audit itself.
    sources.retain(|(path, _)| path.file_name().is_none_or(|name| name != "cutover.rs"));
    assert!(
        !sources.is_empty(),
        "the source scan must actually find files"
    );
    sources
}

/// The vocabulary the split removed. A match anywhere in this crate means a
/// deleted concept came back under its old name.
const REMOVED: &[&str] = &[
    // The private IPC and its frame protocol.
    "ResidentSocket",
    "SupervisorFeed",
    "supervisor_socket_path",
    "ConnectionRole",
    "CommandSessionId",
    "HandshakeRequest",
    "HandshakeReply",
    "SUPERVISOR_PROTOCOL_VERSION",
    // The private bootstrap handshake and its environment.
    "PHOXAL_RESIDENT_BOOTSTRAP_FD",
    "PHOXAL_RESIDENT_EXECUTION_ID",
    "has_private_bootstrap",
    "private_bootstrap_execution",
    "report_private_bootstrap",
    "BootstrapResult",
    "launch_detached",
    // The in-process daemon.
    "run_resident_supervision",
    "should_run_resident_in_process",
    "live_simulate_setup",
    "SupervisorSnapshotV0",
    // The identity axis a single execution made redundant.
    "supervisor_generation",
    "graph_generation",
    // The process-global project root and the duplicate offline reader.
    "PHOXAL_PROJECT_ROOT",
    "PROJECT_ROOT_ENV",
    "offline_from_env",
    // Manual-input capability on the wire.
    "ManualInput",
    "manual_input_from_staged_root",
];

/// The crates the split deleted outright. A dependency edge to either would
/// mean the private protocol or the client library is back.
const REMOVED_CRATES: &[&str] = &["phoxal_cli_protocol", "phoxal_cli_supervisor"];

#[test]
fn no_deleted_resident_or_generation_symbol_survives() {
    let sources = sources();
    for symbol in REMOVED {
        let offenders: Vec<_> = sources
            .iter()
            .filter(|(_, text)| text.contains(symbol))
            .map(|(path, _)| path.display().to_string())
            .collect();
        assert!(
            offenders.is_empty(),
            "`{symbol}` was deleted by the phoxal/phoxald split but appears in: {offenders:?}"
        );
    }
}

#[test]
fn the_client_never_links_the_daemon_or_the_deleted_protocol() {
    let sources = sources();
    for crate_name in REMOVED_CRATES {
        let offenders: Vec<_> = sources
            .iter()
            .filter(|(_, text)| text.contains(crate_name))
            .map(|(path, _)| path.display().to_string())
            .collect();
        assert!(
            offenders.is_empty(),
            "`{crate_name}` no longer exists as a client dependency; found in: {offenders:?}"
        );
    }
    let manifest =
        std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml"))
            .expect("the client manifest is readable");
    for crate_name in ["phoxal-cli-protocol", "phoxal-cli-supervisor"] {
        assert!(
            !manifest.contains(crate_name),
            "`{crate_name}` must not be a dependency of the client"
        );
    }
    // Bin-only: nothing may link this package as a library again.
    assert!(
        !manifest.contains("[lib]"),
        "the client is bin-only; a library target would let the daemon link it"
    );
}
