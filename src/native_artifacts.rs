const SCOPE_DIGEST_FILE: &str = ".phoxal-sha256";

mod preflight;
pub(crate) use preflight::{
    ArtifactProgressReporter, descriptors, ensure_vendored_completeness,
    prepare_descriptors_with_preflight, stage_runtime, stage_tool,
};
mod bundles;
pub(crate) use bundles::stage_component_bundles_into_robot_root;
mod stage;
pub(crate) use stage::stage_descriptor;
mod lock;
pub(crate) use lock::ArtifactStoreLock;
#[cfg(windows)]
pub(crate) use lock::WindowsOverlapped;
pub(crate) use lock::{try_advisory_lock, unlock_advisory};
mod prepare;
pub(crate) use prepare::{PreparedScope, prepare_descriptor, prepare_scope_candidate};
mod activation;
pub(crate) use activation::{
    prepare_and_activate_descriptors, prepare_and_activate_descriptors_with_progress,
};
mod paths;
#[cfg(test)]
pub(crate) use paths::active_version;
pub(crate) use paths::{
    active_version_for, active_version_read_only, artifact_assets_dir_for, artifact_binary_path,
    artifact_exec_dir, artifact_tarball_path, artifact_target_dir_for, descriptor_is_current,
    existing_target_scopes,
};
mod download;
pub(crate) use download::{download_blob, download_blob_inner};
mod retarget;
pub(crate) use retarget::{retarget_active, retarget_active_version};
mod publication;
pub(crate) use publication::ScopePublication;
mod prune;
pub(crate) use prune::{activate_packages_atomically, inactive_versions, prune_inactive_versions};
mod archive;
pub(crate) use archive::{unpack_archive, unpack_asset};
mod storage;
pub(crate) use storage::{
    artifact_package_dir, descriptor_scope_label, validate_path_segment, write_file_atomic,
};
mod suite_store;
pub(crate) use suite_store::{load_vendored_suite, persist_suite};

#[cfg(test)]
mod tests;
