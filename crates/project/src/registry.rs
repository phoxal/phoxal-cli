//! Read-only public registry inspection for a locked framework train.

use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use phoxal_cli_core::project::train::RegistryStatus;
use serde::Deserialize;

pub fn inspect_registry_train(version: &str) -> Result<RegistryStatus> {
    let url = format!("https://crates.io/api/v1/crates/phoxal/{version}");
    let response = reqwest::blocking::Client::builder()
        .user_agent("phoxal-cli")
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(5))
        .build()?
        .get(&url)
        .send()
        .with_context(|| format!("failed to inspect crates.io facade surface {url}"))?;
    let status = response.status();
    if status == reqwest::StatusCode::NOT_FOUND {
        bail!(
            "framework train {version} is not yet published on the crates.io phoxal facade surface ({url}); retry after publication completes"
        );
    }
    ensure!(
        status.is_success(),
        "crates.io facade surface {url} returned HTTP {status}; retry without changing Cargo.lock"
    );
    parse_registry_status(&response.text()?)
}

fn parse_registry_status(body: &str) -> Result<RegistryStatus> {
    #[derive(Deserialize)]
    struct Response {
        version: Version,
    }
    #[derive(Deserialize)]
    struct Version {
        yanked: bool,
    }
    let response: Response =
        serde_json::from_str(body).context("crates.io returned malformed version metadata")?;
    Ok(if response.version.yanked {
        RegistryStatus::Yanked
    } else {
        RegistryStatus::Available
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_metadata_distinguishes_yanked_locked_train() {
        assert_eq!(
            parse_registry_status(r#"{"version":{"yanked":true}}"#).unwrap(),
            RegistryStatus::Yanked
        );
        assert_eq!(
            parse_registry_status(r#"{"version":{"yanked":false}}"#).unwrap(),
            RegistryStatus::Available
        );
    }
}
