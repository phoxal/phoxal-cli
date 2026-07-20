//! Tests for this module.

use super::download::verify_blob_bytes;
use super::storage::package_storage_key;
use super::*;
use crate::host_paths::test_support::ScratchPhoxalHome;
use anyhow::{Context, Result};
use phoxal_cli_core::artifacts::NativeArtifactDescriptor;
use phoxal_cli_core::project::catalog::ArtifactKind;
use sha2::{Digest, Sha256};
use std::fs;

fn descriptor(version: &str, bytes: &[u8]) -> NativeArtifactDescriptor {
    NativeArtifactDescriptor {
        package_id: "phoxal/service-drive".to_string(),
        kind: ArtifactKind::Service,
        name: "drive".to_string(),
        version: version.to_string(),
        url: "https://example.invalid/drive.tar".to_string(),
        sha256: hex::encode(Sha256::digest(bytes)),
        size: bytes.len() as u64,
        binary_name: "phoxal-service-drive".to_string(),
        target: Some("aarch64-unknown-linux-musl".to_string()),
    }
}

fn mark_current(descriptor: &NativeArtifactDescriptor) -> Result<()> {
    let root = artifact_exec_dir(descriptor)?;
    fs::create_dir_all(&root)?;
    fs::write(root.join(SCOPE_DIGEST_FILE), &descriptor.sha256)?;
    Ok(())
}

#[test]
fn blob_size_and_sha_are_both_enforced() {
    let bytes = b"verified";
    let descriptor = descriptor("1.0.0", bytes);
    verify_blob_bytes(&descriptor, bytes).unwrap();
    assert!(verify_blob_bytes(&descriptor, b"wrong").is_err());
    let mut wrong_sha = descriptor;
    wrong_sha.sha256 = "0".repeat(64);
    assert!(verify_blob_bytes(&wrong_sha, bytes).is_err());
}

#[test]
fn active_symlink_selects_the_retargeted_version() -> Result<()> {
    let _root = ScratchPhoxalHome::new()?;
    let old = descriptor("1.0.0", b"old");
    let new = descriptor("2.0.0", b"new");
    fs::create_dir_all(artifact_exec_dir(&old)?)?;
    fs::create_dir_all(artifact_exec_dir(&new)?)?;
    retarget_active(&new)?;
    assert_eq!(active_version(&new)?.as_deref(), Some("2.0.0"));
    Ok(())
}

#[test]
fn activation_prunes_only_after_the_new_set_is_ready() -> Result<()> {
    let _root = ScratchPhoxalHome::new()?;
    let old = descriptor("1.0.0", b"old");
    let new = descriptor("2.0.0", b"new");
    mark_current(&old)?;
    mark_current(&new)?;
    retarget_active(&old)?;

    prepare_and_activate_descriptors(std::slice::from_ref(&new), None)?;

    assert_eq!(active_version(&new)?.as_deref(), Some("2.0.0"));
    assert!(!artifact_exec_dir(&old)?.exists());
    assert!(artifact_exec_dir(&new)?.exists());
    Ok(())
}

#[test]
fn failed_prepare_preserves_the_active_version_and_inactive_fallback() -> Result<()> {
    let _root = ScratchPhoxalHome::new()?;
    let old = descriptor("1.0.0", b"old");
    mark_current(&old)?;
    retarget_active(&old)?;
    let mut new = descriptor("2.0.0", b"new");
    new.url = "http://127.0.0.1:1/drive.tar".to_string();

    assert!(prepare_and_activate_descriptors(std::slice::from_ref(&new), None).is_err());

    assert_eq!(active_version(&old)?.as_deref(), Some("1.0.0"));
    assert!(artifact_exec_dir(&old)?.exists());
    Ok(())
}

#[test]
fn failed_multi_package_refresh_preserves_every_active_scope_and_link() -> Result<()> {
    let _root = ScratchPhoxalHome::new()?;
    let old_drive = descriptor("1.0.0", b"old-drive");
    let mut old_motion = descriptor("1.0.0", b"old-motion");
    old_motion.package_id = "phoxal/service-motion".to_string();
    mark_current(&old_drive)?;
    mark_current(&old_motion)?;
    fs::write(
        artifact_exec_dir(&old_drive)?.join(&old_drive.binary_name),
        "old-drive",
    )?;
    fs::write(
        artifact_exec_dir(&old_motion)?.join(&old_motion.binary_name),
        "old-motion",
    )?;
    retarget_active(&old_drive)?;
    retarget_active(&old_motion)?;

    let archive = minimal_tar_gz(&old_drive.binary_name, b"new-drive")?;
    let addr = spawn_minimal_http_server(archive.clone());
    let mut refreshed_drive = descriptor("1.0.0", &archive);
    refreshed_drive.url = format!("http://{addr}/drive.tar.gz");
    let mut failed_motion = descriptor("2.0.0", b"new-motion");
    failed_motion.package_id = old_motion.package_id.clone();
    failed_motion.url = "http://127.0.0.1:1/motion.tar.gz".to_string();

    assert!(prepare_and_activate_descriptors(&[refreshed_drive, failed_motion], None).is_err());

    assert_eq!(active_version(&old_drive)?.as_deref(), Some("1.0.0"));
    assert_eq!(active_version(&old_motion)?.as_deref(), Some("1.0.0"));
    assert_eq!(
        fs::read_to_string(artifact_exec_dir(&old_drive)?.join(&old_drive.binary_name))?,
        "old-drive"
    );
    assert_eq!(
        fs::read_to_string(artifact_exec_dir(&old_motion)?.join(&old_motion.binary_name))?,
        "old-motion"
    );
    Ok(())
}

#[test]
fn local_identity_is_validated_and_filesystem_safe() -> Result<()> {
    // Matches `filesystem_safe_package_name` used everywhere else in the
    // system, so a package maps to the same on-disk name in the store, the
    // resolver, the deploy install plan, and the framework's release tags.
    assert_eq!(
        package_storage_key("phoxal/service-drive")?,
        ("phoxal".to_string(), "service-drive".to_string())
    );
    assert!(package_storage_key("../service-drive").is_err());
    assert!(package_storage_key("phoxal/service/drive").is_err());

    let mut invalid = descriptor("../escape", b"bytes");
    assert!(artifact_exec_dir(&invalid).is_err());
    invalid.version = "1.0.0".to_string();
    invalid.target = Some("../../escape".to_string());
    assert!(artifact_exec_dir(&invalid).is_err());
    Ok(())
}

#[test]
fn layout_is_provider_scoped_and_version_atomic() -> Result<()> {
    let _root = ScratchPhoxalHome::new()?;
    let target = descriptor("1.2.3", b"target");
    let mut assets = target.clone();
    assets.kind = ArtifactKind::ComponentAssets;
    assets.binary_name.clear();
    assets.target = None;

    assert!(artifact_exec_dir(&target)?.ends_with(
        "artifacts/phoxal/service-drive/versions/1.2.3/targets/aarch64-unknown-linux-musl"
    ));
    assert!(
        artifact_exec_dir(&assets)?
            .ends_with("artifacts/phoxal/service-drive/versions/1.2.3/assets")
    );
    Ok(())
}

#[test]
fn lock_file_self_heals_and_is_removed_after_exclusive_work() -> Result<()> {
    let _root = ScratchPhoxalHome::new()?;
    let lock_path = crate::host_paths::artifacts_dir()?.join(".lock");
    fs::create_dir_all(lock_path.parent().context("lock has no parent")?)?;
    fs::write(&lock_path, b"stale")?;

    let lock = ArtifactStoreLock::exclusive("test")?;
    assert!(lock_path.is_file());
    drop(lock);

    assert!(!lock_path.exists());
    Ok(())
}

#[test]
fn last_shared_holder_removes_lock_file() -> Result<()> {
    let _root = ScratchPhoxalHome::new()?;
    let lock_path = crate::host_paths::artifacts_dir()?.join(".lock");
    let first = ArtifactStoreLock::shared()?;
    let second = ArtifactStoreLock::shared()?;

    drop(first);
    assert!(lock_path.is_file());
    drop(second);

    assert!(!lock_path.exists());
    Ok(())
}

/// Finding A3: a warm cache (every actionable descriptor already staged)
/// must emit NO `download` phase - Product decision 3 forbids showing a
/// phase for work that never runs. Uses a descriptor whose exec dir is
/// pre-created so `prepare_descriptor` takes its `MissingOnly` early
/// return and never reaches the network, regardless of the (unreachable)
/// URL.
#[tokio::test]
async fn prepare_descriptors_with_preflight_emits_no_download_phase_on_a_warm_cache() -> Result<()>
{
    let _diagnostics_guard = crate::session::diagnostics::DIAGNOSTICS_TEST_LOCK
        .lock()
        .await;
    let _root = ScratchPhoxalHome::new()?;
    let mut staged = descriptor("1.0.0", b"already-staged");
    staged.url = "http://127.0.0.1:1/drive.tar".to_string();
    mark_current(&staged)?;

    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    crate::session::diagnostics::install(tx);

    prepare_descriptors_with_preflight(std::slice::from_ref(&staged), None)?;

    crate::session::diagnostics::uninstall();
    while let Ok(event) = rx.try_recv() {
        assert!(
            !matches!(
                event,
                phoxal_cli_core::session::event::SessionEvent::PhaseStarted { .. }
                    | phoxal_cli_core::session::event::SessionEvent::PhaseFinished { .. }
            ),
            "a warm cache must not emit a download phase, got {event:?}"
        );
    }
    Ok(())
}

/// The fresh-cache counterpart: a descriptor with no staged exec dir must
/// genuinely attempt a download, so a `download` phase must appear -
/// started AND finished, even though the download itself fails (an
/// unroutable localhost port stands in for "no network available",
/// keeping this test fast and deterministic without a real artifact
/// server).
#[tokio::test]
async fn prepare_descriptors_with_preflight_emits_a_download_phase_on_a_cold_cache() -> Result<()> {
    let _diagnostics_guard = crate::session::diagnostics::DIAGNOSTICS_TEST_LOCK
        .lock()
        .await;
    let _root = ScratchPhoxalHome::new()?;
    let mut cold = descriptor("1.0.0", b"never-staged");
    cold.url = "http://127.0.0.1:1/drive.tar".to_string();

    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    crate::session::diagnostics::install(tx);

    let result = prepare_descriptors_with_preflight(std::slice::from_ref(&cold), None);
    crate::session::diagnostics::uninstall();
    assert!(result.is_err(), "an unroutable download must fail");

    let mut saw_started = false;
    let mut saw_finished_failed = false;
    while let Ok(event) = rx.try_recv() {
        match event {
            phoxal_cli_core::session::event::SessionEvent::PhaseStarted { id, .. }
                if id.as_str() == "download" =>
            {
                saw_started = true;
            }
            phoxal_cli_core::session::event::SessionEvent::PhaseFinished {
                id, outcome, ..
            } if id.as_str() == "download" => {
                assert!(
                    matches!(
                        outcome,
                        phoxal_cli_core::session::event::PhaseOutcome::Failed { .. }
                    ),
                    "the download phase must report its real failure, got {outcome:?}"
                );
                saw_finished_failed = true;
            }
            _ => {}
        }
    }
    assert!(saw_started, "a cold cache must start a download phase");
    assert!(
        saw_finished_failed,
        "a cold cache's failed download must still finish its phase"
    );
    Ok(())
}

/// A minimal local HTTP/1.1 server that serves `body` for exactly one
/// request, then exits - just enough for a real `reqwest::blocking`
/// download to succeed without reaching any external network. The
/// returned `JoinHandle` is intentionally left unjoined: the server
/// thread exits on its own once it has served its one request.
fn spawn_minimal_http_server(body: Vec<u8>) -> std::net::SocketAddr {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind local http server");
    let addr = listener.local_addr().expect("local server address");
    std::thread::spawn(move || {
        let Ok((mut stream, _)) = listener.accept() else {
            return;
        };
        let mut buf = [0_u8; 1024];
        let _ = stream.read(&mut buf);
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        let _ = stream.write_all(response.as_bytes());
        let _ = stream.write_all(&body);
        let _ = stream.flush();
    });
    addr
}

/// Build a real, minimal `.tar.gz` archive containing one flat file named
/// `entry_name` - enough for `unpack_asset`'s real `tar -xf` (via the
/// system `tar` binary) to succeed for real, unlike a fake byte blob.
fn minimal_tar_gz(entry_name: &str, contents: &[u8]) -> Result<Vec<u8>> {
    let mut tar_bytes = Vec::new();
    {
        let mut builder = tar::Builder::new(&mut tar_bytes);
        let mut header = tar::Header::new_gnu();
        header.set_size(contents.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        builder.append_data(&mut header, entry_name, contents)?;
        builder.finish()?;
    }
    let mut gz_bytes = Vec::new();
    {
        let mut encoder = flate2::write::GzEncoder::new(&mut gz_bytes, flate2::Compression::fast());
        std::io::Write::write_all(&mut encoder, &tar_bytes)?;
        encoder.finish()?;
    }
    Ok(gz_bytes)
}

#[test]
fn same_version_with_a_new_catalog_digest_is_refreshed() -> Result<()> {
    let _root = ScratchPhoxalHome::new()?;
    let old = descriptor("1.0.0", b"old-catalog-blob");
    mark_current(&old)?;
    fs::write(artifact_exec_dir(&old)?.join(&old.binary_name), "old")?;
    retarget_active(&old)?;

    let archive = minimal_tar_gz(&old.binary_name, b"new")?;
    let addr = spawn_minimal_http_server(archive.clone());
    let mut refreshed = descriptor("1.0.0", &archive);
    refreshed.url = format!("http://{addr}/drive.tar.gz");

    prepare_and_activate_descriptors(std::slice::from_ref(&refreshed), None)?;

    assert!(descriptor_is_current(&refreshed));
    assert_eq!(
        fs::read(artifact_exec_dir(&refreshed)?.join(&refreshed.binary_name))?,
        b"new"
    );
    assert_eq!(active_version(&refreshed)?.as_deref(), Some("1.0.0"));
    Ok(())
}

/// Finding C2: `PhaseProgress` used to be constructed only by a render
/// test, never by production code. This exercises the REAL download
/// pipeline end to end (a genuine HTTP download of a real tar.gz archive,
/// unpacked by the system `tar`) and asserts a real
/// `SessionEvent::PhaseProgress` for the "download" phase comes out the
/// other end, not just `PhaseStarted`/`PhaseFinished`.
#[tokio::test]
async fn prepare_descriptors_with_preflight_emits_real_download_progress_on_success() -> Result<()>
{
    let _diagnostics_guard = crate::session::diagnostics::DIAGNOSTICS_TEST_LOCK
        .lock()
        .await;
    let _root = ScratchPhoxalHome::new()?;

    let archive_bytes = minimal_tar_gz("phoxal-service-drive", b"#!/bin/sh\n")?;
    let addr = spawn_minimal_http_server(archive_bytes.clone());
    let mut fresh = descriptor("1.0.0", &archive_bytes);
    fresh.url = format!("http://{addr}/drive.tar.gz");

    let (tx, mut rx) = tokio::sync::mpsc::channel(16);
    crate::session::diagnostics::install(tx);
    let result = prepare_descriptors_with_preflight(std::slice::from_ref(&fresh), None);
    crate::session::diagnostics::uninstall();
    result?;

    assert!(descriptor_is_current(&fresh));

    let mut saw_progress = false;
    while let Ok(event) = rx.try_recv() {
        if let phoxal_cli_core::session::event::SessionEvent::PhaseProgress {
            id,
            completed,
            total,
            ..
        } = event
        {
            assert_eq!(id.as_str(), "download");
            assert_eq!(completed, 1);
            assert_eq!(total, 1);
            saw_progress = true;
        }
    }
    assert!(
        saw_progress,
        "a real successful download must emit real PhaseProgress, not just Started/Finished"
    );
    Ok(())
}
