//! Runtime-bundle verification shared by build and archive validation.

use std::path::Path;

use crate::check::participant_metadata::{ExpectedTarget, inspect_selected_binary_for_target};
use anyhow::{Context, Result};
use phoxal_bundle::RuntimeBundle;

/// Open the authoritative runtime document, verify all indexed content, and
/// prove every executable is for the requested target and carries the exact
/// embedded contract persisted for it.
pub(crate) fn validate_runtime_bundle(
    root: &Path,
    expected: ExpectedTarget,
) -> Result<RuntimeBundle> {
    let bundle = RuntimeBundle::open_verified(root)
        .context("failed to verify runtime.json and indexed bundle content")?;
    for (id, artifact) in bundle.artifacts() {
        let binary = root.join(artifact.path().as_str());
        let embedded = inspect_selected_binary_for_target(&binary, &expected)
            .with_context(|| format!("failed to inspect runtime artifact '{id}'"))?;
        anyhow::ensure!(
            &embedded == artifact.contract(),
            "runtime artifact '{id}' embedded contract differs from runtime.json"
        );
    }
    Ok(bundle)
}
