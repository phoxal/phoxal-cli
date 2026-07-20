#[cfg(test)]
use phoxal_cli_core::deploy::target_for_arch;
pub use phoxal_cli_core::deploy::{
    DownloadArtifact, DownloadDescriptor, OfficialDelivery, TargetTriples,
};

const OPT_ROOT: &str = "/opt/phoxal";
const ACTIVE_ROOT: &str = "/opt/phoxal/active";
const OPT_BIN: &str = "/opt/phoxal/active/bin";
const OPT_ENV: &str = "/opt/phoxal/active/env";
const RELEASES_ROOT: &str = "/opt/phoxal/releases";
const SYSTEMD_DIR: &str = "/etc/systemd/system";
const IDENTITY_DIR: &str = "/var/lib/phoxal/identity";
const HELPER_PATH: &str = "/usr/local/sbin/phoxal-systemd-helper";
const SUDOERS_PATH: &str = "/etc/sudoers.d/phoxal-deploy";
const PAYLOAD_STAGING_PREFIX: &str = "/tmp/phoxal-payload-";
const RELEASE_SCHEMA: &str = "phoxal.release/v0";
const DOWNLOAD_DESCRIPTOR_SCHEMA: &str = "phoxal.deploy-download/v0";
const DOWNLOAD_CONCURRENCY: usize = 4;
const DOWNLOAD_RETRIES: usize = 3;
const SUDO_PASSWORD_ENV: &str = "PHOXAL_SUDO_PASSWORD";
const COMPONENT_FILE: &str = "component.yaml";
const STRUCTURE_FILE: &str = "structure.urdf";
const SIMULATION_FILE: &str = "simulation.yaml";
const MESHES_DIR: &str = "meshes";
const WATCHDOG_SEC: u64 = 10;
const CARGO_ZIGBUILD_VERSION: &str = "0.23.0";

mod models;
pub(crate) use models::{
    BootstrapScripts, DeployOptions, DeployTransport, LocalSudoPasswordSource,
    OfficialArtifactPlan, ReleaseArtifact, ReleaseRecord, RemoteProbe, RenderedPayload,
    SourceBuildArtifact, SudoPassword, SudoPasswordSource,
};
pub use models::{
    Deploy, DeployReport, HealthReport, HealthUnitReport, IdentityInstallPlan, InstallPlan,
};
mod command;
mod execute;
pub(crate) use execute::{
    deploy_command, deploy_ssh_command, deploy_with_transport, local_tty_available,
    read_password_from_tty, sudo_bootstrap_args, sudo_password_from_env, sudo_validate_args,
};
mod target;
pub(crate) use target::{DRY_RUN_REMOTE_USER, parse_deploy_host, validate_deploy_options};
mod prepare;
pub(crate) use prepare::prepare_deploy;
mod payload;
pub(crate) use payload::{
    RenderPayloadInput, payload_bin, payload_env, payload_opt, payload_systemd, render_payload,
    run_bounded, stage_official_fallback, validate_download_descriptor,
};
mod source;
pub(crate) use source::{ensure_no_native_c_source_dependencies, stage_source_artifacts};
mod toolchain;
pub(crate) use toolchain::{ZigbuildToolchain, ensure_zigbuild_toolchain};
#[cfg(test)]
pub(crate) use toolchain::{path_with_cache_bin, validate_zigbuild_toolchain};
mod cross_build;
pub(crate) use cross_build::cross_build_source_binary;
mod official;
pub(crate) use official::{
    official_runtime_by_artifact_id, official_runtimes_including_component_drivers,
    stage_official_artifacts,
};
mod env_units;
pub(crate) use env_units::{render_env_files, render_units};
mod participant_units;
pub(crate) use participant_units::{
    UnitPrivileges, participant_binary_name, participant_unit, participant_unit_name,
    unit_privileges,
};
mod metadata;
pub(crate) use metadata::{stage_payload_metadata, write_robot_yaml};
mod release;
pub(crate) use release::{helper_script_sha256, release_record, sha256_file, write_text};
mod bootstrap;
pub(crate) use bootstrap::{
    bootstrap_script, format_health_failure, helper_script, managed_unit_name, report,
    report_from_payload, stale_units, sudoers_fragment, validate_remote_username,
};
mod ssh;
pub(crate) use ssh::SshTransport;
mod sync;
#[cfg(test)]
pub(crate) use sync::{
    PayloadSyncRemote, participant_from_unit, remote_staging_dir, sync_payload_via_helper,
};

#[cfg(test)]
mod tests;
