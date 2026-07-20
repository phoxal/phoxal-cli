//! Payload layout, download descriptors, and bounded staging.

use super::{
    ACTIVE_ROOT, BootstrapScripts, DOWNLOAD_CONCURRENCY, DOWNLOAD_DESCRIPTOR_SCHEMA,
    DOWNLOAD_RETRIES, DRY_RUN_REMOTE_USER, DownloadArtifact, DownloadDescriptor, HELPER_PATH,
    InstallPlan, OfficialArtifactPlan, RenderedPayload, SUDOERS_PATH, TargetTriples, WATCHDOG_SEC,
    helper_script, release_record, render_env_files, render_units, stage_official_artifacts,
    stage_payload_metadata, stage_source_artifacts, sudoers_fragment, validate_remote_username,
    write_robot_yaml, write_text,
};
use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use anyhow::bail;
use phoxal::model::robot::v0::Robot;
use phoxal_cli_core::project::launch_plan::LaunchPlan;
use phoxal_cli_core::project::launch_plan::PlanContext;
use phoxal_cli_core::project::tooling::make_executable;
use serde_json::Value;
use sha2::Digest;
use sha2::Sha256;
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

pub(crate) struct RenderPayloadInput<'a> {
    pub(crate) robot: &'a Robot,
    pub(crate) ctx: &'a PlanContext,
    pub(crate) plan: &'a LaunchPlan,
    pub(crate) target: TargetTriples,
    pub(crate) health_timeout: Duration,
    pub(crate) remote_user: &'a str,
    pub(crate) ui: &'a crate::Ui,
}

pub(crate) fn render_payload(input: RenderPayloadInput<'_>) -> Result<RenderedPayload> {
    let RenderPayloadInput {
        robot,
        ctx,
        plan,
        target,
        health_timeout,
        remote_user,
        ui,
    } = input;
    let project_root = ctx.project_root.as_path();
    let resolved = &ctx.resolved;
    let source_participants = ctx.source_participants.as_slice();
    // The hostless `--dry-run` sentinel is never installed anywhere (see its
    // doc comment), so it is exempt from the real-username charset check.
    if remote_user != DRY_RUN_REMOTE_USER {
        validate_remote_username(remote_user)?;
    }
    let root = tempfile::tempdir().context("failed to create deploy payload directory")?;
    create_payload_dirs(root.path())?;

    let bootstrap = BootstrapScripts {
        helper_script: helper_script(),
        sudoers_fragment: sudoers_fragment(),
        remote_user: remote_user.to_string(),
    };

    let source_builds = stage_source_artifacts(
        project_root,
        root.path(),
        resolved,
        source_participants,
        plan,
        &target,
        ui,
    )?;
    let official_plans = stage_official_artifacts(root.path(), resolved, plan, false)?;

    let identity_files = Vec::new();
    let mut env_files = BTreeMap::new();
    render_env_files(root.path(), plan, &mut env_files)?;

    let mut rendered_units = BTreeMap::new();
    let unit_names = render_units(
        root.path(),
        resolved,
        plan,
        &source_builds,
        &official_plans,
        &mut rendered_units,
    )?;

    write_robot_yaml(root.path(), robot)?;
    let metadata_files = stage_payload_metadata(project_root, root.path(), robot, resolved)?;

    let download_descriptor = download_descriptor(&official_plans);
    let descriptor_text = serde_json::to_string_pretty(&download_descriptor)
        .context("failed to encode deploy download descriptor")?;
    write_text(
        &payload_opt(root.path()).join("download-descriptor.json"),
        &(descriptor_text + "\n"),
    )?;

    let release = release_record(resolved, plan, &source_builds, &official_plans)?;
    let release_json_text =
        serde_json::to_string_pretty(&release).context("failed to encode release record")?;
    write_text(
        &payload_opt(root.path()).join("phoxal-release.json"),
        &(release_json_text.clone() + "\n"),
    )?;
    let release_json = serde_json::from_str::<Value>(&release_json_text)?;

    let missing_official_artifacts = official_plans
        .values()
        .filter_map(|artifact| artifact.missing_label.clone())
        .collect::<Vec<_>>();
    let mut direct_writes = vec![
        format!("{ACTIVE_ROOT}/robot.yaml"),
        format!("{ACTIVE_ROOT}/phoxal-release.json"),
    ];
    direct_writes.extend(metadata_files);
    let release_generation = release_generation(&release_json_text)?;
    let install_plan = InstallPlan {
        helper_path: HELPER_PATH.to_string(),
        sudoers_path: SUDOERS_PATH.to_string(),
        scoped_delete: Vec::new(),
        direct_writes,
        identity_files,
        units: unit_names.clone(),
        stale_units_to_remove: Vec::new(),
        lifecycle: vec![
            "daemon-reload".to_string(),
            "enable phoxal.target and generated phoxal-* services".to_string(),
            "restart phoxal.target".to_string(),
            "health report".to_string(),
        ],
        health_deadline_seconds: health_timeout.as_secs(),
        watchdog_sec: WATCHDOG_SEC,
        missing_official_artifacts,
        release_generation,
    };

    let install_plan_text =
        serde_json::to_string_pretty(&install_plan).context("failed to encode install plan")?;
    write_text(
        root.path().join("install-plan.json").as_path(),
        &(install_plan_text + "\n"),
    )?;

    Ok(RenderedPayload {
        root,
        target,
        install_plan,
        rendered_units,
        env_files,
        release_json,
        download_descriptor,
        official_plans,
        delivery: None,
        unit_names,
        bootstrap,
    })
}

pub(crate) fn create_payload_dirs(root: &Path) -> Result<()> {
    for path in [payload_bin(root), payload_env(root), payload_systemd(root)] {
        fs::create_dir_all(&path)
            .with_context(|| format!("failed to create {}", path.display()))?;
    }
    Ok(())
}

pub(crate) fn download_descriptor(
    official_plans: &BTreeMap<String, OfficialArtifactPlan>,
) -> DownloadDescriptor {
    DownloadDescriptor {
        schema: DOWNLOAD_DESCRIPTOR_SCHEMA.to_string(),
        concurrency: DOWNLOAD_CONCURRENCY,
        retries: DOWNLOAD_RETRIES,
        artifacts: official_plans
            .values()
            .map(|artifact| DownloadArtifact {
                package: artifact.artifact_id.clone(),
                version: artifact.version.clone(),
                target: artifact.target.clone(),
                url: artifact.url.clone(),
                size: artifact.size,
                sha256: artifact.sha256.clone(),
                archive_binary_name: artifact.archive_binary_name.clone(),
                install_binary_name: artifact.install_binary_name.clone(),
            })
            .collect(),
    }
}

pub(crate) fn validate_download_descriptor(descriptor: &DownloadDescriptor) -> Result<()> {
    if descriptor.artifacts.is_empty() {
        bail!("resolved deploy has no official artifacts to download");
    }
    for artifact in &descriptor.artifacts {
        if !artifact.url.starts_with("https://")
            || artifact.size == 0
            || !phoxal::catalog::is_sha256(&artifact.sha256)
        {
            bail!(
                "NativePending: official artifact {} {} has no complete immutable blob for {}; run `phoxal update` and verify the catalog publishes this robot target",
                artifact.package,
                artifact.version,
                artifact.target
            );
        }
    }
    Ok(())
}

pub(crate) fn stage_official_fallback(payload: &mut RenderedPayload, ui: &crate::Ui) -> Result<()> {
    let descriptors = payload
        .official_plans
        .values()
        .map(
            |artifact| phoxal_cli_core::artifacts::NativeArtifactDescriptor {
                package_id: artifact.artifact_id.clone(),
                kind: artifact.kind,
                name: artifact.artifact_id.clone(),
                version: artifact.version.clone(),
                url: artifact.url.clone(),
                sha256: artifact.sha256.clone(),
                size: artifact.size,
                binary_name: artifact.archive_binary_name.clone(),
                target: Some(artifact.target.clone()),
            },
        )
        .collect::<Vec<_>>();
    crate::native_artifacts::prepare_descriptors_with_preflight(&descriptors, Some(ui))?;
    let _lock = crate::native_artifacts::ArtifactStoreLock::shared()?;
    for (descriptor, artifact) in descriptors.iter().zip(payload.official_plans.values_mut()) {
        let source = crate::native_artifacts::artifact_binary_path(descriptor)?;
        let dest = payload_bin(payload.root.path()).join(&artifact.install_binary_name);
        fs::copy(&source, &dest).with_context(|| {
            format!(
                "failed to stage fallback artifact {} from {}",
                artifact.artifact_id,
                source.display()
            )
        })?;
        make_executable(&dest)?;
        artifact.source_path = Some(source);
    }
    Ok(())
}

pub(crate) fn run_bounded<T, F>(items: &[T], concurrency: usize, operation: F) -> Result<()>
where
    T: Sync,
    F: Fn(&T) -> Result<()> + Sync,
{
    if concurrency == 0 {
        bail!("download concurrency must be greater than zero");
    }
    for batch in items.chunks(concurrency) {
        std::thread::scope(|scope| -> Result<()> {
            let handles = batch
                .iter()
                .map(|item| scope.spawn(|| operation(item)))
                .collect::<Vec<_>>();
            for handle in handles {
                handle
                    .join()
                    .map_err(|_| anyhow!("robot download worker panicked"))??;
            }
            Ok(())
        })?;
    }
    Ok(())
}

pub(crate) fn release_generation(release_json: &str) -> Result<String> {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before Unix epoch")?
        .as_nanos();
    let mut digest = Sha256::new();
    digest.update(release_json.as_bytes());
    digest.update(nonce.to_le_bytes());
    Ok(hex::encode(digest.finalize())[..16].to_string())
}

pub(crate) fn payload_opt(root: &Path) -> PathBuf {
    root.join("opt/phoxal")
}

pub(crate) fn payload_bin(root: &Path) -> PathBuf {
    root.join("opt/phoxal/bin")
}

pub(crate) fn payload_env(root: &Path) -> PathBuf {
    root.join("opt/phoxal/env")
}

pub(crate) fn payload_systemd(root: &Path) -> PathBuf {
    root.join("opt/phoxal/systemd")
}
