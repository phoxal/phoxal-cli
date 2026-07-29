//! Exact infrastructure-router readiness probing.

use crate::{ManagedChild, ROUTER_READY_TIMEOUT};
use anyhow::{Context, Result};
use std::path::Path;
use std::time::Duration;
use tokio::net::UnixStream;

/// Prove the router's unix-socket endpoint actually accepts a connection -
/// not merely that its process has started or that the socket path exists.
async fn probe_router_endpoint(endpoint: &str) -> Result<()> {
    let path = unixsock_stream_path(endpoint)?;
    let stream = UnixStream::connect(path)
        .await
        .with_context(|| format!("connect to infrastructure router at {endpoint}"))?;
    drop(stream);
    Ok(())
}

pub(super) fn unixsock_stream_path(endpoint: &str) -> Result<&Path> {
    endpoint
        .strip_prefix("unixsock-stream/")
        .map(Path::new)
        .with_context(|| {
            format!(
                "router endpoint {endpoint} is not a unixsock-stream endpoint understood by the readiness probe"
            )
        })
}

pub(super) async fn wait_for_router_connection(
    child: &mut ManagedChild,
    endpoint: &str,
    stderr_tail: &std::sync::Arc<std::sync::Mutex<String>>,
) -> Result<()> {
    let deadline = tokio::time::Instant::now() + ROUTER_READY_TIMEOUT;
    let error = loop {
        if let Some(status) = child.try_wait()? {
            return Err(router_start_error(
                anyhow::anyhow!("infrastructure router exited before the CLI connected ({status})"),
                stderr_tail,
            ));
        }
        let error =
            match tokio::time::timeout(Duration::from_millis(250), probe_router_endpoint(endpoint))
                .await
            {
                Ok(Ok(())) => return Ok(()),
                Ok(Err(error)) => error,
                Err(_) => anyhow::anyhow!("CLI readiness connection attempt timed out"),
            };
        if tokio::time::Instant::now() >= deadline {
            break error;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    Err(router_start_error(
        error.context("timed out waiting for the CLI to connect to the infrastructure router"),
        stderr_tail,
    ))
}

fn router_start_error(
    error: anyhow::Error,
    stderr_tail: &std::sync::Arc<std::sync::Mutex<String>>,
) -> anyhow::Error {
    let tail = stderr_tail
        .lock()
        .map(|tail| tail.clone())
        .unwrap_or_default();
    if tail.is_empty() {
        error
    } else {
        error.context(format!("infrastructure router stderr:\n{tail}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;
    use tokio::net::UnixListener;

    fn scratch_dir(label: &str) -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix(&format!("phoxal-router-gate-{label}-"))
            .tempdir_in("/tmp")
            .expect("create a short-path temp dir for the unix-socket probe test")
    }

    #[tokio::test]
    async fn probe_fails_when_nothing_is_at_the_endpoint() {
        let dir = scratch_dir("missing");
        let endpoint = format!(
            "unixsock-stream/{}",
            dir.path().join("router.sock").display()
        );
        probe_router_endpoint(&endpoint)
            .await
            .expect_err("a socket path nothing has bound must fail the probe");
    }

    #[tokio::test]
    async fn probe_fails_when_the_socket_file_exists_but_nothing_is_listening() {
        let dir = scratch_dir("stale");
        let socket_path = dir.path().join("router.sock");
        {
            let listener =
                std::os::unix::net::UnixListener::bind(&socket_path).expect("bind stale listener");
            drop(listener);
        }
        std::fs::remove_file(&socket_path).expect("remove orphaned socket inode");
        std::fs::write(&socket_path, b"stale").expect("write stale non-socket path");
        assert!(socket_path.exists());
        let endpoint = format!("unixsock-stream/{}", socket_path.display());
        probe_router_endpoint(&endpoint)
            .await
            .expect_err("a stale socket file with no listener must not be reported ready");
    }

    #[tokio::test]
    async fn probe_succeeds_once_a_listener_is_actually_accepting() {
        let dir = scratch_dir("live");
        let socket_path = dir.path().join("router.sock");
        let listener = UnixListener::bind(&socket_path).expect("bind live listener");
        let accept_task = tokio::spawn(async move {
            let _ = listener.accept().await;
        });
        let endpoint = format!("unixsock-stream/{}", socket_path.display());
        let result = probe_router_endpoint(&endpoint).await;
        accept_task.abort();
        result.expect("a live, accepting listener must pass the probe");
    }

    #[tokio::test]
    async fn wait_for_router_connection_does_not_report_ready_before_the_listener_accepts() {
        let dir = scratch_dir("ordering");
        let socket_path = dir.path().join("router.sock");
        let endpoint = format!("unixsock-stream/{}", socket_path.display());
        let mut command = tokio::process::Command::new("sleep");
        command
            .arg("5")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        let mut child =
            ManagedChild::spawn(&mut command, false, &[]).expect("spawn stand-in child process");

        let stderr_tail = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let listen_delay = Duration::from_millis(500);
        let delayed_listener = {
            let socket_path = socket_path.clone();
            tokio::spawn(async move {
                tokio::time::sleep(listen_delay).await;
                let listener =
                    UnixListener::bind(&socket_path).expect("bind delayed router listener");
                let _ = listener.accept().await;
            })
        };

        let started = Instant::now();
        let result = wait_for_router_connection(&mut child, &endpoint, &stderr_tail).await;
        let elapsed = started.elapsed();
        let _ = child.start_kill();
        delayed_listener.abort();

        result.expect("readiness must succeed once the listener actually accepts");
        assert!(
            elapsed >= listen_delay / 2,
            "readiness resolved before the router listener started accepting"
        );
    }
}
