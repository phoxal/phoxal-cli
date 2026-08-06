//! The embedded Zenoh router.
//!
//! The comms fabric is infrastructure the supervisor firsthand owns, so it runs
//! in this process rather than as a supervised child (organization#978). What
//! that deletes is the point: there is no router binary to stage or resolve, no
//! spawn, no readiness probe polling a socket, and no full-graph recovery epoch
//! driven by a child exit. [`phoxal_bus::Router::open`] returning means the
//! endpoint is bound - `phoxal-bus` pins the Zenoh listen settings that make
//! that true - so the router is simply ready or the run failed to start.
//!
//! The router owns no keys and no subscriptions; participants and the
//! supervisor's own observer session reach it as ordinary clients over the
//! endpoint it listens on.

use crate::SupervisorState;
use anyhow::{Context, Result};
use std::path::Path;

/// The running embedded router. Holding it keeps the fabric up; dropping or
/// [`EmbeddedRouter::close`]ing it takes every link down with it.
#[derive(Debug)]
pub struct EmbeddedRouter {
    router: phoxal_bus::Router,
    endpoint: String,
    /// Watches this router from the outside. Closed before the router is, so
    /// an ordinary shutdown is never reported as a loss.
    watch: phoxal_bus::RouterWatch,
}

impl EmbeddedRouter {
    /// The endpoint participants dial.
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// Close the router. Called on the way out of a session, after the graph
    /// has been torn down, so participants lose their links to a router that is
    /// already finished with them rather than mid-shutdown.
    ///
    /// The watch goes first, so a deliberate stop is not reported as the fabric
    /// failing.
    pub async fn close(self) -> Result<()> {
        if let Err(error) = self.watch.close().await {
            tracing::debug!("failed to close the router watch: {error}");
        }
        self.router
            .close()
            .await
            .context("failed to close the embedded router")
    }
}

/// Open the embedded router on `endpoint`.
///
/// `config` is the optional authored Zenoh JSON5 file, resolved by staging into
/// the runtime layout so a source run, a staged run, and an extracted bundle all
/// reach the same asset. Phoxal's transport policy and the listen settings are
/// applied after it by `phoxal-bus`, so an authored file cannot put the router
/// at odds with the participants that dial it.
///
/// `endpoint` must be a plain endpoint string: a per-endpoint config fragment
/// (`tcp/…#exit_on_failure=false`) would override the pinned listen settings
/// that make a successful open mean "bound".
pub async fn start_embedded_router(
    execution: phoxal_cli_core::identity::ExecutionId,
    endpoint: String,
    config: Option<&Path>,
    board: SupervisorState,
) -> Result<EmbeddedRouter> {
    anyhow::ensure!(
        !endpoint.contains('#'),
        "router endpoint {endpoint} carries a per-endpoint config fragment; that would override \
         the listen settings which make a successful open mean the endpoint is bound"
    );
    if let Some(socket) = unixsock_stream_path(&endpoint)
        && let Some(parent) = socket.parent()
    {
        std::fs::create_dir_all(parent).with_context(|| {
            format!(
                "failed to create the router socket directory {}",
                parent.display()
            )
        })?;
    }
    // One execution equals one router lifetime: the router's ZID IS the
    // execution id, so a client that reads the router id has learned the
    // execution without asking anyone.
    let router = phoxal_bus::Router::open(execution, std::slice::from_ref(&endpoint), config)
        .await
        .with_context(|| format!("failed to open the embedded router on {endpoint}"))?;

    // The router runs in this process, so nothing else would notice it going
    // away: participants would simply go stale a Liveliness lease later, and
    // the supervisor would keep reporting a graph it can no longer reach. The
    // watch dials the router from the outside and answers the one question the
    // router's own session cannot answer about itself.
    let lost_endpoint = endpoint.clone();
    let watch = phoxal_bus::RouterWatch::open(&endpoint, move || {
        tracing::error!("the router at {lost_endpoint} is gone; the robot graph is unreachable");
        board.fail(&format!(
            "the embedded router at {lost_endpoint} went away while the session was running"
        ));
    })
    .await
    .with_context(|| format!("failed to watch the embedded router on {endpoint}"))?;

    Ok(EmbeddedRouter {
        router,
        endpoint,
        watch,
    })
}

/// The filesystem path behind a `unixsock-stream/` endpoint, if it is one.
/// Zenoh binds the socket itself but will not create missing parent
/// directories, so the caller creates them first.
fn unixsock_stream_path(endpoint: &str) -> Option<&Path> {
    endpoint.strip_prefix("unixsock-stream/").map(Path::new)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_unix_socket_endpoint_yields_its_path() {
        assert_eq!(
            unixsock_stream_path("unixsock-stream//tmp/phoxal/router.sock"),
            Some(Path::new("/tmp/phoxal/router.sock"))
        );
        assert_eq!(unixsock_stream_path("tcp/127.0.0.1:7447"), None);
    }

    #[tokio::test]
    async fn an_endpoint_config_fragment_is_rejected() {
        // A fragment can set `exit_on_failure=false`, which would send binding
        // to a background retry task and make a successful open mean nothing.
        let error = start_embedded_router(
            "tcp/127.0.0.1:7447#exit_on_failure=false".into(),
            None,
            SupervisorState::new(),
        )
        .await
        .expect_err("a per-endpoint config fragment must be rejected");
        assert!(error.to_string().contains("config fragment"), "{error:#}");
    }

    // Zenoh refuses to run on Tokio's current-thread scheduler, so any test
    // that actually opens a router needs the multi-thread flavour. The shipped
    // binary is fine: `#[tokio::main]` in `src/main.rs` is multi-thread.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn opening_creates_the_socket_directory_and_binds() {
        let dir = tempfile::Builder::new()
            .prefix("phoxal-embedded-router-")
            .tempdir_in("/tmp")
            .expect("short-path temp dir for the unix socket");
        // Deliberately a directory staging has not created yet.
        let socket = dir.path().join("run").join("router.sock");
        let endpoint = format!("unixsock-stream/{}", socket.display());

        let router = start_embedded_router(endpoint.clone(), None, SupervisorState::new())
            .await
            .expect("the router creates its socket directory and binds");
        assert_eq!(router.endpoint(), endpoint);
        assert!(
            socket.exists(),
            "a successful open must mean the endpoint is bound, not merely requested"
        );
        router.close().await.expect("close the embedded router");
    }

    /// The whole point of the watch: if the fabric disappears under a running
    /// session, the supervisor learns immediately rather than waiting for every
    /// participant to go stale.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn losing_the_router_fails_the_session() {
        let dir = tempfile::Builder::new()
            .prefix("phoxal-router-loss-")
            .tempdir_in("/tmp")
            .expect("short-path temp dir for the unix socket");
        let endpoint = format!("unixsock-stream/{}", dir.path().join("r.sock").display());
        let board = SupervisorState::new();

        let router = start_embedded_router(endpoint, None, board.clone())
            .await
            .expect("router opens");
        assert!(
            board.supervisor_snapshot().failure.is_none(),
            "a healthy router must not fail the session"
        );

        // Drop the router without closing the watch first - the failure this
        // exists to catch, not the orderly shutdown `close` performs.
        let EmbeddedRouter { router, watch, .. } = router;
        router.close().await.expect("close the router");

        let mut failed = false;
        for _ in 0..100 {
            if board.supervisor_snapshot().failure.is_some() {
                failed = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(failed, "losing the router must fail the session");
        assert!(
            board
                .supervisor_snapshot()
                .failure
                .expect("failure recorded")
                .contains("went away"),
            "the failure must say the router went away"
        );
        let _ = watch.close().await;
    }
}
