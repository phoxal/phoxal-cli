use anyhow::Result;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub(crate) enum StagingBuild {
    HostRuntime,
    NativeBundle {
        target: String,
        prebuilt_target_dir: Option<PathBuf>,
        officials_source: Option<PathBuf>,
    },
}

impl StagingBuild {
    pub(crate) fn host_runtime() -> Self {
        Self::HostRuntime
    }

    pub(crate) fn native_bundle(target: String) -> Self {
        Self::NativeBundle {
            target,
            prebuilt_target_dir: None,
            officials_source: None,
        }
    }

    pub(crate) fn prebuilt_native_bundle(
        target: String,
        target_dir: PathBuf,
        officials_source: Option<PathBuf>,
    ) -> Self {
        Self::NativeBundle {
            target,
            prebuilt_target_dir: Some(target_dir),
            officials_source,
        }
    }

    pub(crate) fn include_simulators(&self) -> bool {
        matches!(self, Self::HostRuntime)
    }

    pub(crate) fn target(&self) -> Option<&str> {
        match self {
            Self::HostRuntime => None,
            Self::NativeBundle { target, .. } => Some(target),
        }
    }

    pub(crate) fn officials_source(&self) -> Option<&Path> {
        match self {
            Self::HostRuntime => None,
            Self::NativeBundle {
                officials_source, ..
            } => officials_source.as_deref(),
        }
    }

    pub(crate) fn source_profile(&self) -> Profile {
        match self {
            Self::HostRuntime => Profile::Debug,
            Self::NativeBundle { .. } => Profile::Release,
        }
    }

    pub(crate) fn prebuilt_target_dir(&self) -> Option<&Path> {
        match self {
            Self::HostRuntime
            | Self::NativeBundle {
                prebuilt_target_dir: None,
                ..
            } => None,
            Self::NativeBundle {
                prebuilt_target_dir: Some(target_dir),
                ..
            } => Some(target_dir),
        }
    }

    pub(crate) fn materialize_settings(
        &self,
        project_root: &Path,
        offline: bool,
    ) -> Result<crate::stage::MaterializeSettings> {
        let target_dir = super::cargo::cargo_target_dir(project_root, offline)?;
        Ok(match self {
            Self::HostRuntime => crate::stage::MaterializeSettings::development(target_dir),
            Self::NativeBundle { .. } => crate::stage::MaterializeSettings::release(target_dir),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Profile {
    Debug,
    Release,
}

impl Profile {
    pub(super) const fn dir_name(self) -> &'static str {
        match self {
            Self::Debug => "debug",
            Self::Release => "release",
        }
    }
}
