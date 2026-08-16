//! Bundle verification shared by build and archive validation.

use std::path::Path;

use crate::check::participant_metadata::{ExpectedTarget, inspect_selected_binary_for_target};
use crate::runtimes::robot_binaries;
use anyhow::{Context, Result};
use phoxal_bundle::RuntimeBundle;

/// Open the one persisted document and prove every executable the robot
/// launches is present, built for the requested target, and declaring the
/// identity it is launched under.
///
/// The bundle itself verifies nothing beyond "the manifest parses" - there is
/// no index and no digest table to check it against, and deliberately so. This
/// is the CLI's own check over a bundle it is about to ship or run, and it
/// reads the binaries rather than a second record of them.
pub(crate) fn validate_runtime_bundle(
    root: &Path,
    expected: ExpectedTarget,
) -> Result<RuntimeBundle> {
    let bundle = RuntimeBundle::open(root).context("failed to read manifest.json")?;
    for binary in robot_binaries(bundle.robot()) {
        let path = root.join(phoxal_bundle::BIN_DIR).join(&binary);
        let embedded = inspect_selected_binary_for_target(&path, &expected)
            .with_context(|| format!("failed to inspect bundle runtime '{binary}'"))?;
        anyhow::ensure!(
            embedded.id.as_str() == binary,
            "bin/{binary} declares participant '{}', but the manifest launches it as '{binary}'",
            embedded.id
        );
    }
    Ok(bundle)
}
