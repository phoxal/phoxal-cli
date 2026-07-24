//! Vendored suite persistence for the strictly-offline execution path (#936).
//!
//! `run`, `start`, `build`, and `watch` never touch the network: they resolve
//! the robot graph against the artifact suite `phoxal update` already fetched and
//! persisted into the project-local `.phoxal/artifacts` store, alongside the
//! artifact blobs it vendored. `phoxal update` is the only code path that fetches
//! and activates a suite; every execution path consumes the persisted copy.

use anyhow::{Context, Result, ensure};
use phoxal_cli_core::project::suite::Suite;
use std::path::PathBuf;

/// The vendored suite descriptor path for a locked train version, under
/// `.phoxal/artifacts/suite/<version>.json`.
fn vendored_suite_path(version: &str) -> Result<PathBuf> {
    super::storage::validate_path_segment("suite version", version)?;
    Ok(crate::host_paths::artifacts_dir()?
        .join("suite")
        .join(format!("{version}.json")))
}

/// Persist the fetched `suite` into the vendored store so the offline execution
/// paths can resolve against it with no network access. Called by `phoxal
/// update` (the only fetcher/activator) after it vendors the artifact blobs.
pub(crate) fn persist_suite(suite: &Suite) -> Result<()> {
    let path = vendored_suite_path(&suite.version)?;
    let bytes =
        serde_json::to_vec_pretty(suite).context("failed to serialize the vendored suite")?;
    super::storage::write_file_atomic(&path, &bytes)
        .with_context(|| format!("failed to persist vendored suite {}", path.display()))?;
    Ok(())
}

/// Load the suite `phoxal update` persisted for `version`, or `None` when the
/// store has none. Never touches the network; the version tag is verified so a
/// mismatched persisted suite is a hard error rather than a silent train mix.
pub(crate) fn load_vendored_suite(version: &str) -> Result<Option<Suite>> {
    let path = vendored_suite_path(version)?;
    if !path.exists() {
        return Ok(None);
    }
    let suite = phoxal_cli_core::project::suite::read_suite_path(&path)
        .with_context(|| format!("vendored suite {} is not a valid suite", path.display()))?;
    ensure!(
        suite.version == version,
        "vendored suite {} declares version {}, but the locked train is {version}; run `phoxal update`",
        path.display(),
        suite.version
    );
    Ok(Some(suite))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host_paths::test_support::ScratchPhoxalHome;
    use phoxal_cli_core::project::suite::fixture_suite_for_tests;

    #[test]
    fn persisted_suite_round_trips_from_the_vendored_store() -> Result<()> {
        let _scratch = ScratchPhoxalHome::new()?;
        let mut suite = fixture_suite_for_tests(Vec::new());
        suite.version = "0.1.0".to_string();

        assert!(load_vendored_suite("0.1.0")?.is_none());
        persist_suite(&suite)?;
        let loaded = load_vendored_suite("0.1.0")?.expect("persisted suite loads back");
        assert_eq!(loaded.version, "0.1.0");
        Ok(())
    }

    #[test]
    fn a_version_mismatched_persisted_suite_is_rejected() -> Result<()> {
        let _scratch = ScratchPhoxalHome::new()?;
        // Write a suite whose declared version disagrees with the filename the
        // loader keys on (a corrupted/hand-edited store), and confirm the loader
        // refuses it rather than mixing trains.
        let mut suite = fixture_suite_for_tests(Vec::new());
        suite.version = "0.1.0".to_string();
        let path = vendored_suite_path("0.2.0")?;
        super::super::storage::write_file_atomic(&path, &serde_json::to_vec(&suite)?)?;

        let error = load_vendored_suite("0.2.0").expect_err("a version mismatch must be rejected");
        assert!(format!("{error:#}").contains("phoxal update"), "{error:#}");
        Ok(())
    }
}
