//! Compatibility declaration embedded in every compiled runtime root.

use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

pub const FILE_NAME: &str = phoxal_cli_core::project::layout::RUNTIME_HEADER_PATH;
pub const SCHEMA: &str = "phoxal.runtime/v0";
pub const RUNTIME_REVISION: u16 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeHeader {
    pub schema: String,
    pub revisions: RuntimeRevisions,
    pub built_with: BuiltWith,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeRevisions {
    pub runtime: u16,
    pub robot: u16,
    pub launch: u16,
    #[serde(rename = "participant-metadata")]
    pub participant_metadata: u16,
    #[serde(rename = "standard-runtime")]
    pub standard_runtime: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BuiltWith {
    pub phoxal: String,
}

impl RuntimeHeader {
    #[cfg(test)]
    #[must_use]
    pub fn current() -> Self {
        Self::for_phoxal_version("test")
    }

    #[must_use]
    pub fn for_phoxal_version(version: &str) -> Self {
        Self {
            schema: SCHEMA.to_string(),
            revisions: RuntimeRevisions {
                runtime: RUNTIME_REVISION,
                robot: 0,
                launch: 0,
                participant_metadata: 0,
                standard_runtime: 0,
            },
            built_with: BuiltWith {
                phoxal: version.to_string(),
            },
        }
    }

    pub fn write_to(&self, root: &Path) -> Result<()> {
        let path = root.join(FILE_NAME);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let bytes = serde_json::to_vec_pretty(self)?;
        std::fs::write(&path, bytes).with_context(|| format!("failed to write {}", path.display()))
    }

    pub fn read_and_validate(root: &Path) -> Result<Self> {
        let path = root.join(FILE_NAME);
        let bytes = std::fs::read(&path).with_context(|| {
            format!(
                "failed to read {}; rebuild the bundle with a CLI that emits the compiled \
                 robot.json/assets/bin runtime format",
                path.display()
            )
        })?;
        let header: Self = serde_json::from_slice(&bytes)
            .with_context(|| format!("invalid {}", path.display()))?;
        header.validate(&path)?;
        Ok(header)
    }

    fn validate(&self, path: &Path) -> Result<()> {
        if self.schema != SCHEMA {
            anyhow::bail!(
                "{} declares unsupported schema `{}`; update the CLI",
                path.display(),
                self.schema
            );
        }
        let revisions = &self.revisions;
        if revisions.runtime != RUNTIME_REVISION
            || [
                revisions.robot,
                revisions.launch,
                revisions.participant_metadata,
                revisions.standard_runtime,
            ]
            .iter()
            .any(|revision| *revision != 0)
        {
            anyhow::bail!(
                "{} declares an unsupported runtime compatibility revision; update the CLI",
                path.display()
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_header_round_trips_strictly() -> Result<()> {
        let root = tempfile::tempdir()?;
        RuntimeHeader::current().write_to(root.path())?;
        assert_eq!(
            RuntimeHeader::read_and_validate(root.path())?,
            RuntimeHeader::current()
        );
        Ok(())
    }

    #[test]
    fn unsupported_revision_requires_a_cli_update() -> Result<()> {
        let root = tempfile::tempdir()?;
        let mut header = RuntimeHeader::current();
        header.revisions.launch = 1;
        header.write_to(root.path())?;
        let error = RuntimeHeader::read_and_validate(root.path()).unwrap_err();
        assert!(format!("{error:#}").contains("update the CLI"));
        Ok(())
    }

    #[test]
    fn legacy_root_header_requires_rebuilding_the_bundle() -> Result<()> {
        let root = tempfile::tempdir()?;
        std::fs::write(
            root.path().join("phoxal.runtime.json"),
            serde_json::to_vec_pretty(&RuntimeHeader::current())?,
        )?;
        let error = RuntimeHeader::read_and_validate(root.path())
            .expect_err("a pre-revision layout must not be mistaken for the new format");
        let message = format!("{error:#}");
        assert!(message.contains("assets/runtime.json"), "{message}");
        assert!(message.contains("rebuild the bundle"), "{message}");
        Ok(())
    }

    #[test]
    fn build_version_is_provenance_not_a_compatibility_gate() -> Result<()> {
        let root = tempfile::tempdir()?;
        let mut header = RuntimeHeader::current();
        header.built_with.phoxal = "0.0.0-provenance-only".to_string();
        header.write_to(root.path())?;
        assert_eq!(
            RuntimeHeader::read_and_validate(root.path())?
                .built_with
                .phoxal,
            "0.0.0-provenance-only"
        );
        Ok(())
    }
}
