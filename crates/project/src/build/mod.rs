pub(crate) mod builder_image;
pub(crate) mod cargo;
pub(crate) mod container;
pub(crate) mod materialise;
pub(crate) mod profile;
pub(crate) mod shell;
mod use_case;

pub use use_case::build_bundle;
#[cfg(test)]
pub(crate) use use_case::resolve_container_staging;

use std::path::PathBuf;
use std::sync::Arc;

use crate::RuntimeTarget;

use crate::Reporter;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BuildBackend {
    Local {
        target: Option<String>,
    },
    Container {
        target: Option<String>,
        engine: container::ContainerEngine,
        image: Option<String>,
    },
    Ssh {
        host: String,
        target: Option<String>,
    },
}

pub struct BuildBundleRequest {
    pub target: RuntimeTarget,
    pub backend: BuildBackend,
    /// Where the release this build packages gets the `phoxald` for its target.
    pub executor: crate::deployment::SharedExecutorSource,
    pub output: Option<PathBuf>,
    pub publish: bool,
    pub offline: bool,
    pub reporter: Arc<dyn Reporter>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuiltBundle {
    pub archive: PathBuf,
    pub sha256: String,
    /// The deployment release this archive was written from, when the build was
    /// asked to publish one into the project.
    pub release_root: Option<PathBuf>,
}
