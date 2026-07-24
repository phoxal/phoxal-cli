//! Release manifest construction, checksums, and timestamp formatting.

use super::{
    OfficialArtifactPlan, RELEASE_SCHEMA, ReleaseArtifact, ReleaseRecord, SourceBuildArtifact,
    helper_script, official_runtime_by_artifact_id,
};
use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use phoxal_cli_core::project::launch_plan::INFRASTRUCTURE_ROUTER;
use phoxal_cli_core::project::launch_plan::LaunchPlan;
use phoxal_cli_core::project::launch_plan::ParticipantExecution;
use phoxal_cli_core::project::resolver::ResolvedRobot;
use serde_json::Value;
use sha2::Digest;
use sha2::Sha256;
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

pub(crate) fn release_record(
    resolved: &ResolvedRobot,
    plan: &LaunchPlan,
    source_builds: &BTreeMap<String, SourceBuildArtifact>,
    official_plans: &BTreeMap<String, OfficialArtifactPlan>,
) -> Result<ReleaseRecord> {
    let mut artifacts = BTreeMap::<String, ReleaseArtifact>::new();
    if let Some(router) = official_plans.get(INFRASTRUCTURE_ROUTER) {
        artifacts.insert(
            router.artifact_id.clone(),
            release_official_artifact(router),
        );
    }

    for participant in &plan.robots[0].participants {
        match &participant.execution {
            ParticipantExecution::OfficialArtifact { .. } => {
                let runtime = official_runtime_by_artifact_id(resolved, &participant.artifact_id)
                    .ok_or_else(|| {
                    anyhow!("missing runtime for {}", participant.artifact_id)
                })?;
                if let Some(artifact) = official_plans.get(&runtime.package) {
                    artifacts
                        .entry(artifact.artifact_id.clone())
                        .or_insert_with(|| release_official_artifact(artifact));
                }
            }
            ParticipantExecution::OfficialTool { .. } => {
                if let Some(artifact) = official_plans.get(&participant.artifact_id) {
                    artifacts
                        .entry(artifact.artifact_id.clone())
                        .or_insert_with(|| release_official_artifact(artifact));
                }
            }
            ParticipantExecution::UserService { .. } => {
                if let Some(artifact) = source_builds.get(&participant.artifact_id) {
                    artifacts
                        .entry(artifact.artifact_id.clone())
                        .or_insert_with(|| release_source_artifact(artifact));
                }
            }
            ParticipantExecution::SourceArtifact { kind, .. } if kind == "service" => {
                let id = format!("service-{}", participant.artifact_id);
                if let Some(artifact) = source_builds.get(&id) {
                    artifacts
                        .entry(artifact.artifact_id.clone())
                        .or_insert_with(|| release_source_artifact(artifact));
                }
            }
            ParticipantExecution::SourceArtifact { kind, .. } if kind == "tool" => {
                if let Some(artifact) = source_builds.get(&participant.artifact_id) {
                    artifacts
                        .entry(artifact.artifact_id.clone())
                        .or_insert_with(|| release_source_artifact(artifact));
                }
            }
            ParticipantExecution::SourceArtifact { kind, .. } => {
                let id = format!("{kind}-{}", participant.artifact_id);
                if let Some(artifact) = source_builds.get(&id) {
                    artifacts
                        .entry(artifact.artifact_id.clone())
                        .or_insert_with(|| release_source_artifact(artifact));
                }
            }
            ParticipantExecution::ComponentDriver { .. } => {
                let id = format!("driver-{}", participant.artifact_id);
                if let Some(artifact) = source_builds.get(&id) {
                    artifacts
                        .entry(artifact.artifact_id.clone())
                        .or_insert_with(|| release_source_artifact(artifact));
                }
            }
        }
    }

    Ok(ReleaseRecord {
        schema: RELEASE_SCHEMA.to_string(),
        created_at_utc: utc_now_string()?,
        artifacts: artifacts.into_values().collect(),
    })
}

pub(crate) fn release_source_artifact(artifact: &SourceBuildArtifact) -> ReleaseArtifact {
    let _ = &artifact.payload_path;
    ReleaseArtifact {
        id: artifact.artifact_id.clone(),
        kind: artifact.kind.to_string(),
        version: None,
        source: artifact.source.clone(),
        sha256: artifact.sha256.clone(),
        target: None,
        url: None,
    }
}

pub(crate) fn release_official_artifact(artifact: &OfficialArtifactPlan) -> ReleaseArtifact {
    ReleaseArtifact {
        id: artifact.artifact_id.clone(),
        kind: artifact.kind.to_string(),
        version: Some(artifact.version.clone()),
        source: Value::String("suite".to_string()),
        sha256: artifact.sha256.clone(),
        target: Some(artifact.target.clone()),
        url: Some(artifact.url.clone()),
    }
}

pub(crate) fn utc_now_string() -> Result<String> {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before Unix epoch")?
        .as_secs() as i64;
    Ok(format_unix_utc(secs))
}

pub(crate) fn format_unix_utc(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let seconds_of_day = secs.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let hour = seconds_of_day / 3_600;
    let minute = (seconds_of_day % 3_600) / 60;
    let second = seconds_of_day % 60;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

pub(crate) fn civil_from_days(days_since_epoch: i64) -> (i32, u32, u32) {
    let z = days_since_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = mp + if mp < 10 { 3 } else { -9 };
    let year = y + if m <= 2 { 1 } else { 0 };
    (year as i32, m as u32, d as u32)
}

pub(crate) fn sha256_file(path: &Path) -> Result<String> {
    let bytes = fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
    Ok(hex::encode(Sha256::digest(bytes)))
}

pub(crate) fn helper_script_sha256() -> String {
    hex::encode(Sha256::digest(helper_script().as_bytes()))
}

pub(crate) fn write_text(path: &Path, contents: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    fs::write(path, contents).with_context(|| format!("failed to write {}", path.display()))
}
