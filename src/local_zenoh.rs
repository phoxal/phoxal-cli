use anyhow::{Context, Result, bail};

use crate::shell;

pub const ZENOH_IMAGE: &str =
    "eclipse/zenoh:1.9.0@sha256:157965d71e0bfd0a044d76a985ff0e5c306ad3968929168fb9678cd2a7fec23f";

pub const LOCAL_ZENOH_NETWORK: &str = "phoxal-link";
pub const LOCAL_ZENOH_CONTAINER: &str = "phoxal-local-zenoh";
pub const LOCAL_ZENOH_PORT: u16 = 7447;

#[must_use]
pub fn build_run_args() -> Vec<String> {
    vec![
        "run".to_string(),
        "-d".to_string(),
        "--rm".to_string(),
        "--name".to_string(),
        LOCAL_ZENOH_CONTAINER.to_string(),
        "--network".to_string(),
        LOCAL_ZENOH_NETWORK.to_string(),
        "--publish".to_string(),
        format!("127.0.0.1:{LOCAL_ZENOH_PORT}:{LOCAL_ZENOH_PORT}"),
        "--init".to_string(),
        ZENOH_IMAGE.to_string(),
        "-l".to_string(),
        format!("tcp/0.0.0.0:{LOCAL_ZENOH_PORT}"),
        "--no-multicast-scouting".to_string(),
        "--cfg".to_string(),
        "mode:\"router\"".to_string(),
    ]
}

pub fn start_if_absent() -> Result<()> {
    let output = shell::run_stdout(
        "docker",
        [
            "ps",
            "--filter",
            &format!("name=^{LOCAL_ZENOH_CONTAINER}$"),
            "--format",
            "{{.Names}}",
        ],
        None,
    )
    .context("failed to inspect local zenoh container")?;
    if output.trim().is_empty() {
        shell::run_status("docker", build_run_args(), None)
            .context("failed to start local zenoh container")?;
    }
    Ok(())
}

pub fn stop() -> Result<()> {
    let output = shell::run_output("docker", ["rm", "-f", LOCAL_ZENOH_CONTAINER], None)
        .context("failed to stop local zenoh container")?;
    if output.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    if stderr.contains("No such container") && stderr.contains(LOCAL_ZENOH_CONTAINER) {
        return Ok(());
    }

    bail!(
        "`docker rm -f {}` failed with status {}\nstdout:\n{}\nstderr:\n{}",
        LOCAL_ZENOH_CONTAINER,
        output.status,
        String::from_utf8_lossy(&output.stdout),
        stderr
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_run_args_emits_expected_argv() {
        assert_eq!(
            build_run_args(),
            vec![
                "run".to_string(),
                "-d".to_string(),
                "--rm".to_string(),
                "--name".to_string(),
                "phoxal-local-zenoh".to_string(),
                "--network".to_string(),
                "phoxal-link".to_string(),
                "--publish".to_string(),
                "127.0.0.1:7447:7447".to_string(),
                "--init".to_string(),
                ZENOH_IMAGE.to_string(),
                "-l".to_string(),
                "tcp/0.0.0.0:7447".to_string(),
                "--no-multicast-scouting".to_string(),
                "--cfg".to_string(),
                "mode:\"router\"".to_string(),
            ]
        );
    }

    #[test]
    fn zenoh_image_uses_pinned_1_9_0_digest() {
        assert!(ZENOH_IMAGE.starts_with("eclipse/zenoh:1.9.0@sha256:"));
        assert_eq!(ZENOH_IMAGE.len(), 91);
    }
}
