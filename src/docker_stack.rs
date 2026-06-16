use std::path::Path;

use anyhow::{Context, Result};

use crate::shell;

pub(crate) fn bring_up_stack(compose_path: &Path) -> Result<()> {
    ensure_link_network()?;
    // phoxal-local-zenoh is a singleton; concurrent `phoxal simulate` runs are
    // unsupported because either teardown can stop the shared container. A
    // future `phoxal local up/down` command will expose explicit lifecycle.
    crate::local_zenoh::start_if_absent()?;
    compose_up(compose_path)
}

pub(crate) fn tear_down_stack(compose_path: &Path) -> Result<()> {
    compose_down(compose_path)?;
    crate::local_zenoh::stop()?;
    remove_link_network_best_effort();
    Ok(())
}

fn ensure_link_network() -> Result<()> {
    let network = crate::local_zenoh::LOCAL_ZENOH_NETWORK;
    let output = shell::run_output("docker", ["network", "inspect", network], None)
        .context("failed to inspect phoxal link network")?;
    if output.status.success() {
        return Ok(());
    }
    shell::run_status(
        "docker",
        ["network", "create", "--driver", "bridge", network],
        None,
    )
    .context("failed to create phoxal link network")
}

fn remove_link_network_best_effort() {
    let network = crate::local_zenoh::LOCAL_ZENOH_NETWORK;
    let Ok(output) = shell::run_output("docker", ["network", "rm", network], None) else {
        tracing::debug!("failed to run docker network rm {network}");
        return;
    };
    if output.status.success() {
        return;
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    if stderr.contains("has active endpoints") || stderr.contains("No such network") {
        tracing::debug!("leaving docker network {network}: {}", stderr.trim());
        return;
    }

    tracing::debug!(
        "`docker network rm {}` failed with status {}\nstdout:\n{}\nstderr:\n{}",
        network,
        output.status,
        String::from_utf8_lossy(&output.stdout),
        stderr
    );
}

fn compose_up(compose_path: &Path) -> Result<()> {
    let compose_arg = compose_path.to_string_lossy().to_string();
    shell::run_status(
        "docker",
        ["compose", "-f", compose_arg.as_str(), "up", "-d", "--wait"],
        None,
    )
}

fn compose_down(compose_path: &Path) -> Result<()> {
    let compose_arg = compose_path.to_string_lossy().to_string();
    shell::run_status(
        "docker",
        ["compose", "-f", compose_arg.as_str(), "down"],
        None,
    )
}
