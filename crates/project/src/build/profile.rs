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

    pub(crate) fn build_user_binary(
        &self,
        crate_dir: &Path,
        preferred_name: &str,
        reporter: &dyn crate::Reporter,
        offline: bool,
    ) -> Result<PathBuf> {
        match self {
            Self::HostRuntime => super::cargo::build_source_binary(
                crate_dir,
                preferred_name,
                reporter,
                None,
                offline,
            ),
            Self::NativeBundle {
                target,
                prebuilt_target_dir: None,
                ..
            } => super::cargo::build_source_binary_with_profile(
                crate_dir,
                preferred_name,
                reporter,
                Some(target),
                Profile::Release,
                offline,
            ),
            Self::NativeBundle {
                target,
                prebuilt_target_dir: Some(target_dir),
                ..
            } => super::cargo::locate_prebuilt_binary(
                crate_dir,
                preferred_name,
                target_dir,
                Some(target),
                Profile::Release,
            ),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
