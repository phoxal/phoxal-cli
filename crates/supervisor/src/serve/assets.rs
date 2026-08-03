//! Declared-asset serving, answered by the supervisor itself.
//!
//! `service/asset` used to be a participant that re-served files the supervisor
//! had already staged - pure indirection (organization#978). The supervisor
//! stages the asset root, so it answers `supervisor/asset/get` directly.
//!
//! Two properties come free from that, rather than being maintained:
//!
//! - **Fencing.** [`AssetResolver`] discovers the tree once and only ever
//!   consults that map, so `robot.json`, participant binaries, and anything
//!   outside the asset root are unreachable because they are not keys - not
//!   because a request is inspected for traversal.
//! - **Clocklessness.** The supervisor is not a graph participant, so an asset
//!   query leaves logical and simulation time by construction. This server
//!   publishes nothing and holds no clock.

use anyhow::{Context, Result};
use phoxal_api::v0_1::supervisor::asset::{GetRequest, GetResponse};
use phoxal_bus::{Bus, Codec, ContractBody, MessagePack, QueryFailure};
use phoxal_model::{AssetId, AssetResolver};

/// Serve `supervisor/asset/get` on `bus` until the session ends.
///
/// Returns only when the bus closes; the caller supervises it like any other
/// long-lived session task.
pub(crate) async fn run(bus: &Bus, assets: &AssetResolver) -> Result<()> {
    let topic = <GetRequest as ContractBody>::TOPIC;
    let queryable = bus
        .declare_server(topic)
        .await
        .with_context(|| format!("failed to declare the asset server on {topic}"))?;
    tracing::debug!(
        topic,
        declared_assets = assets.ids().len(),
        "supervisor is serving declared assets"
    );

    loop {
        let query = match queryable.recv().await {
            Ok(query) => query,
            // The bus closed: the session is ending, which is not a fault.
            Err(error) => {
                tracing::debug!("asset server stopped: {error}");
                return Ok(());
            }
        };

        let response = match query
            .request_bytes()
            .map_err(|error| error.to_string())
            .and_then(|bytes| {
                MessagePack::decode::<GetRequest>(&bytes).map_err(|error| error.to_string())
            }) {
            Ok(request) => resolve(assets, &request.path),
            Err(detail) => {
                // A malformed request is the caller's bug, not a missing asset,
                // so it takes Zenoh's error leg rather than being flattened
                // into `Missing`.
                let failure = QueryFailure::internal(format!("malformed asset request: {detail}"));
                if let Err(error) = query.reply_err(&failure).await {
                    tracing::debug!("failed to reject a malformed asset request: {error}");
                }
                continue;
            }
        };

        match MessagePack::encode(&response) {
            Ok(payload) => {
                if let Err(error) = query.reply(bus, payload).await {
                    tracing::debug!("failed to answer an asset query: {error}");
                }
            }
            Err(error) => {
                tracing::warn!("failed to encode an asset response: {error}");
            }
        }
    }
}

/// Resolve a requested logical id against the declared asset set.
///
/// This is the exact behaviour `service/asset` had: an unusable id is
/// `InvalidPath`, an id that is simply not declared is `Missing`. The
/// distinction matters to a caller - one is a bug in the request, the other is
/// a fact about the bundle.
fn resolve(assets: &AssetResolver, path: &str) -> GetResponse {
    let Ok(id) = AssetId::new(path.trim()) else {
        return GetResponse::InvalidPath;
    };
    match assets.read(&id) {
        Ok(bytes) => GetResponse::Found { bytes },
        Err(_) => GetResponse::Missing,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn staged_bundle() -> tempfile::TempDir {
        let root = tempfile::tempdir().expect("temp dir");
        let assets = root.path().join("assets");
        std::fs::create_dir_all(assets.join("meshes")).expect("create asset dirs");
        std::fs::write(assets.join("meshes/base.stl"), b"mesh").expect("write asset");
        // The files the fencing exists to protect, in their real positions.
        std::fs::write(root.path().join("robot.json"), b"secret").expect("write robot.json");
        std::fs::create_dir_all(root.path().join("bin")).expect("create bin");
        std::fs::write(root.path().join("bin/phoxal-service-drive"), b"elf").expect("write binary");
        root
    }

    #[test]
    fn a_declared_asset_is_found() {
        let root = staged_bundle();
        let assets = AssetResolver::discover(root.path().join("assets")).expect("discover");
        assert_eq!(
            resolve(&assets, "meshes/base.stl"),
            GetResponse::Found {
                bytes: b"mesh".to_vec()
            }
        );
        // Surrounding whitespace is tolerated, exactly as before the absorption.
        assert!(matches!(
            resolve(&assets, "  meshes/base.stl  "),
            GetResponse::Found { .. }
        ));
    }

    #[test]
    fn traversal_and_unusable_ids_are_rejected_not_merely_missing() {
        let root = staged_bundle();
        let assets = AssetResolver::discover(root.path().join("assets")).expect("discover");
        for path in ["", "   ", "../robot.json", "a/../b", "/etc/passwd", "a\\b"] {
            assert_eq!(
                resolve(&assets, path),
                GetResponse::InvalidPath,
                "{path:?} must be rejected as a path, not reported as missing"
            );
        }
    }

    #[test]
    fn the_bundles_own_files_are_unreachable() {
        let root = staged_bundle();
        let assets = AssetResolver::discover(root.path().join("assets")).expect("discover");
        // These are syntactically valid ids, so they reach the resolver - and
        // are still unreachable, because they were never discovered as assets.
        for path in [
            "robot.json",
            "bin/phoxal-service-drive",
            "meshes/undeclared.stl",
        ] {
            assert_eq!(
                resolve(&assets, path),
                GetResponse::Missing,
                "{path:?} must not be readable through the asset contract"
            );
        }
    }

    #[test]
    fn a_bundle_with_no_assets_answers_missing_rather_than_failing() {
        let root = tempfile::tempdir().expect("temp dir");
        let assets = AssetResolver::discover(root.path().join("assets")).expect("discover");
        assert_eq!(resolve(&assets, "anything"), GetResponse::Missing);
    }
}
