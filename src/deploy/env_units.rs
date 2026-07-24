//! Environment files and top-level systemd unit rendering.

use super::{
    OPT_BIN, OPT_ENV, OfficialArtifactPlan, SYSTEMD_DIR, SourceBuildArtifact,
    participant_binary_name, participant_unit, participant_unit_name, payload_env, payload_systemd,
    unit_privileges, write_text,
};
use crate::supervisor::START_LIMIT_BURST;
use crate::supervisor::START_LIMIT_INTERVAL;
use anyhow::Context;
use anyhow::Result;
use phoxal_cli_core::project::launch_plan::INFRASTRUCTURE_ROUTER;
use phoxal_cli_core::project::launch_plan::LaunchPlan;
use phoxal_cli_core::project::launch_plan::ParticipantExecution;
use phoxal_cli_core::project::resolver::ResolvedRobot;
use phoxal_cli_core::session::launch_env::EncodedParticipantEnv;
use phoxal_cli_core::session::launch_env::{encode_participant_env, encode_tool_env};
use std::collections::BTreeMap;
use std::path::Path;

pub(crate) fn render_env_files(
    root: &Path,
    plan: &LaunchPlan,
    env_files: &mut BTreeMap<String, String>,
) -> Result<()> {
    let robot = plan
        .robots
        .first()
        .context("deploy launch plan has no robot")?;

    for participant in &robot.participants {
        let tool_execution = match &participant.execution {
            ParticipantExecution::OfficialTool { .. } => true,
            ParticipantExecution::SourceArtifact { kind, .. } => kind == "tool",
            _ => false,
        };
        let encoded = if tool_execution {
            encode_tool_env(&participant.launch)?
        } else {
            encode_participant_env(&participant.launch)?
        };
        write_env_file(
            root,
            &format!("{}.env", participant.launch.participant_id),
            &encoded,
            env_files,
        )?;
    }
    Ok(())
}

pub(crate) fn write_env_file(
    root: &Path,
    file_name: &str,
    encoded: &EncodedParticipantEnv,
    env_files: &mut BTreeMap<String, String>,
) -> Result<()> {
    let rendered = encoded.environment_file();
    write_text(&payload_env(root).join(file_name), &rendered)?;
    env_files.insert(format!("{OPT_ENV}/{file_name}"), rendered);
    Ok(())
}

pub(crate) fn render_units(
    root: &Path,
    resolved: &ResolvedRobot,
    plan: &LaunchPlan,
    source_builds: &BTreeMap<String, SourceBuildArtifact>,
    official_plans: &BTreeMap<String, OfficialArtifactPlan>,
    rendered_units: &mut BTreeMap<String, String>,
) -> Result<Vec<String>> {
    let mut unit_names = Vec::new();
    write_unit(root, "phoxal.target", &target_unit(), rendered_units)?;
    unit_names.push("phoxal.target".to_string());

    let router_binary = official_plans
        .get(INFRASTRUCTURE_ROUTER)
        .map(|tool| tool.install_binary_name.clone())
        .unwrap_or_else(|| INFRASTRUCTURE_ROUTER.to_string());
    write_unit(
        root,
        "phoxal-router.service",
        &router_unit(&router_binary),
        rendered_units,
    )?;
    unit_names.push("phoxal-router.service".to_string());

    let robot = plan
        .robots
        .first()
        .context("deploy launch plan has no robot")?;
    for participant in &robot.participants {
        let unit_name = participant_unit_name(&participant.launch.participant_id);
        let binary = participant_binary_name(participant, resolved, source_builds, official_plans)?;
        let privileges = unit_privileges(resolved, &participant.launch.participant_id);
        write_unit(
            root,
            &unit_name,
            &participant_unit(participant, &binary, &privileges),
            rendered_units,
        )?;
        unit_names.push(unit_name);
    }
    Ok(unit_names)
}

pub(crate) fn write_unit(
    root: &Path,
    unit_name: &str,
    contents: &str,
    rendered_units: &mut BTreeMap<String, String>,
) -> Result<()> {
    write_text(&payload_systemd(root).join(unit_name), contents)?;
    rendered_units.insert(format!("{SYSTEMD_DIR}/{unit_name}"), contents.to_string());
    Ok(())
}

pub(crate) fn target_unit() -> String {
    "[Unit]\nDescription=Phoxal robot\nWants=phoxal-router.service\n\n[Install]\nWantedBy=multi-user.target\n".to_string()
}

pub(crate) fn router_unit(binary: &str) -> String {
    format!(
        "[Unit]\nDescription=Phoxal Zenoh router\nAfter=network-online.target\nWants=network-online.target\nPartOf=phoxal.target\nStartLimitIntervalSec={}\nStartLimitBurst={START_LIMIT_BURST}\n\n[Service]\nType=simple\nExecStart={OPT_BIN}/{binary} --listen tcp/localhost:7447\nRestart=on-failure\nRestartSec=2s\nTimeoutStopSec=5s\nUser=phoxal\nGroup=phoxal\nNoNewPrivileges=true\n\n[Install]\nWantedBy=phoxal.target\n",
        START_LIMIT_INTERVAL.as_secs()
    )
}
