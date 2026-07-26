//! Tests for this module.

use super::download::{ensure_download_allowed, verify_blob_bytes};
use super::storage::package_storage_key;
use super::*;
use crate::host_paths::test_support::ScratchPhoxalHome;
use anyhow::{Context, Result};
use phoxal_cli_core::artifacts::NativeArtifactDescriptor;
use phoxal_cli_core::project::suite::ArtifactKind;
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
fn offline_mode_rejects_missing_vendored_artifacts_before_download() {
    let descriptor = descriptor("1.0.0", b"missing");
    let error = ensure_download_allowed(&descriptor, true).unwrap_err();
    let message = error.to_string();
    assert!(
        message.contains("offline mode cannot download"),
        "{message}"
    );
    assert!(message.contains("phoxal update"), "{message}");
    assert!(ensure_download_allowed(&descriptor, false).is_ok());
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
fn local_identity_is_validated_and_filesystem_safe() -> Result<()> {
    // Matches `filesystem_safe_package_name` used everywhere else in the
    // system, so a package maps to the same on-disk name in the store, the
    // resolver, and the framework's release tags.
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
