//! Environment files and top-level systemd unit rendering.

use super::{
    ACTIVE_ROOT, OPT_BIN, OPT_ENV, OfficialArtifactPlan, SYSTEMD_DIR, SourceBuildArtifact,
    UnitPrivileges, WATCHDOG_SEC, participant_binary_name, participant_unit, participant_unit_name,
    payload_env, payload_systemd, unit_privileges, write_text,
};
use crate::supervisor::START_LIMIT_BURST;
use crate::supervisor::START_LIMIT_INTERVAL;
use anyhow::Context;
use anyhow::Result;
use phoxal::participant::launch::env;
use phoxal_cli_core::project::launch_plan::DEFAULT_ROUTER_CONNECT;
use phoxal_cli_core::project::launch_plan::LaunchPlan;
use phoxal_cli_core::project::launch_plan::SITE_INFRASTRUCTURE_ROUTER;
use phoxal_cli_core::project::launch_plan::SITE_TOOL_JOYPAD;
use phoxal_cli_core::project::launch_plan::SiteLaunch;
use phoxal_cli_core::project::resolver::ResolvedRobot;
use phoxal_cli_core::session::launch_env::EncodedParticipantEnv;
use phoxal_cli_core::session::launch_env::encode_participant_env;
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
    for site in &plan.site {
        if site.id == SITE_INFRASTRUCTURE_ROUTER {
            continue;
        }
        // Every OTHER standard site tool (`tool-joypad`, `tool-telemetry`) -
        // a real bus client, so unlike the router (transport-only, no
        // `PHOXAL_CONNECT` of its own) it needs the same connect endpoint
        // every regular participant gets from `launch_plan::participant_launch`.
        let encoded = site_tool_env(site, &robot.namespace, &robot.id)?;
        write_env_file(root, &format!("{}.env", site.id), &encoded, env_files)?;
    }

    for participant in &robot.participants {
        let encoded = encode_participant_env(&participant.launch)?;
        write_env_file(
            root,
            &format!("{}.env", participant.launch.participant_id),
            &encoded,
            env_files,
        )?;
    }
    Ok(())
}

/// The env for every standard site tool OTHER than the router (`tool-joypad`,
/// `tool-telemetry`) - a real bus client, unlike the router itself, so it
/// needs `PHOXAL_CONNECT` set to the same `DEFAULT_ROUTER_CONNECT` every
/// regular participant's `ParticipantLaunch.bus.connect_endpoints` carries
/// (`launch_plan::participant_launch`).
pub(crate) fn site_tool_env(
    site: &SiteLaunch,
    namespace: &str,
    robot_id: &str,
) -> Result<EncodedParticipantEnv> {
    let mut variables = BTreeMap::new();
    variables.insert(env::PARTICIPANT_ID.to_string(), site.id.clone());
    variables.insert(env::NAMESPACE.to_string(), namespace.to_string());
    variables.insert(env::ROBOT_ID.to_string(), robot_id.to_string());
    variables.insert(env::ROBOT_ROOT.to_string(), ACTIVE_ROOT.to_string());
    // Configless tools (`phoxal_config == Value::Null`, e.g. joypad/telemetry)
    // must run with `PHOXAL_CONFIG` ABSENT - a unit config (`type Config = ()`)
    // rejects `{}`. Only a tool carrying real config emits the variable.
    if !site.phoxal_config.is_null() {
        variables.insert(
            env::CONFIG.to_string(),
            serde_json::to_string(&site.phoxal_config)
                .with_context(|| format!("failed to encode PHOXAL_CONFIG for {}", site.id))?,
        );
    }
    variables.insert(env::CONNECT.to_string(), DEFAULT_ROUTER_CONNECT.to_string());
    Ok(EncodedParticipantEnv::from_variables(variables))
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
        .get(SITE_INFRASTRUCTURE_ROUTER)
        .map(|tool| tool.install_binary_name.clone())
        .unwrap_or_else(|| SITE_INFRASTRUCTURE_ROUTER.to_string());
    write_unit(
        root,
        "phoxal-router.service",
        &router_unit(&router_binary),
        rendered_units,
    )?;
    unit_names.push("phoxal-router.service".to_string());

    // Every OTHER standard site tool (`tool-joypad`, `tool-telemetry` -
    // CLI-UX Phase 4) gets its own unit too, ordered AFTER the router
    // (`site_tool_unit`'s `After=`/`Wants=`) and matching the staged
    // readiness the CLI supervisor itself uses (router before the other
    // tools - `crate::run::stages_for_run`). `plan.site` already omits
    // `tool-telemetry` entirely when the catalog snapshot in use predates it
    // (`launch_plan::build_site_launches`), so this loop never renders a unit
    // for a tool that was never resolved.
    for site in &plan.site {
        if site.id == SITE_INFRASTRUCTURE_ROUTER {
            continue;
        }
        let unit_name = site_tool_unit_name(&site.id);
        let binary = if source_builds.contains_key(&site.id) {
            site.id.clone()
        } else {
            official_plans
                .get(&site.id)
                .map(|artifact| artifact.install_binary_name.clone())
                .unwrap_or_else(|| site.id.clone())
        };
        let privileges = unit_privileges_for_tool(&site.id);
        write_unit(
            root,
            &unit_name,
            &site_tool_unit(&site.id, &binary, &privileges),
            rendered_units,
        )?;
        unit_names.push(unit_name);
    }

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

/// The unit for a standard site tool OTHER than the router (`tool-joypad`,
/// `tool-telemetry`) - shaped exactly like `participant_unit` (same
/// `Type=notify` readiness contract, same restart/watchdog/hardening
/// defaults: no `MemoryMax`/`CPUQuota` here either, consistent with every
/// other unit this deploy renders) but ordered after the router by unit
/// name rather than a `ParticipantLaunchRecord`, since a site tool has no
/// graph-checked participant record of its own.
///
/// No-controller idle policy (design doc): a site tool with nothing to do
/// yet (`tool-joypad` with no gamepad plugged in) is expected to start and
/// idle cleanly rather than exit - the framework tool itself already stays
/// up in that case (see `tool/joypad`'s own graceful-absence handling), so
/// `Restart=on-failure` never actually flaps for it; this unit adds no
/// additional restart-suppression logic because none is needed.
pub(crate) fn site_tool_unit(id: &str, binary: &str, privileges: &UnitPrivileges) -> String {
    let mut unit = format!(
        "[Unit]\nDescription=Phoxal tool {id}\nAfter=network-online.target phoxal-router.service\nWants=network-online.target\nPartOf=phoxal.target\nStartLimitIntervalSec={}\nStartLimitBurst={START_LIMIT_BURST}\n\n[Service]\nType=notify\nEnvironmentFile={OPT_ENV}/{id}.env\nExecStart={OPT_BIN}/{binary}\n\nRestart=on-failure\nRestartSec=2s\nTimeoutStopSec=5s\nWatchdogSec={WATCHDOG_SEC}s\n\nUser=phoxal\nGroup=phoxal\nNoNewPrivileges=true\n",
        START_LIMIT_INTERVAL.as_secs()
    );
    if !privileges.supplementary_groups.is_empty() {
        unit.push_str("SupplementaryGroups=");
        unit.push_str(&privileges.supplementary_groups.join(" "));
        unit.push('\n');
    }
    if !privileges.device_allow.is_empty() {
        unit.push_str("DevicePolicy=strict\n");
        for device in &privileges.device_allow {
            unit.push_str("DeviceAllow=");
            unit.push_str(device);
            unit.push_str(" rw\n");
        }
    }
    if !privileges.capabilities.is_empty() {
        let caps = privileges.capabilities.join(" ");
        unit.push_str("AmbientCapabilities=");
        unit.push_str(&caps);
        unit.push('\n');
        unit.push_str("CapabilityBoundingSet=");
        unit.push_str(&caps);
        unit.push('\n');
    }
    unit.push_str("\n[Install]\nWantedBy=phoxal.target\n");
    unit
}

pub(crate) fn site_tool_unit_name(id: &str) -> String {
    format!("phoxal-{id}.service")
}

/// The tool-privilege model (CLI-UX Phase 4): a second, hardcoded privilege
/// path for a standard site tool, alongside `unit_privileges`'s
/// component-driver-derived path for a robot participant. `tool-joypad`
/// needs `/dev/input` access to read gamepad hardware - granted
/// unconditionally (the `input` supplementary group plus
/// `DeviceAllow=/dev/input/* rw`) for the current development-grade robots,
/// with no manifest/config switch to gate it (design doc). Every other site
/// tool (`tool-telemetry`; the router has its own always-privilege-free unit)
/// needs no extra grant.
pub(crate) fn unit_privileges_for_tool(tool_id: &str) -> UnitPrivileges {
    if tool_id == SITE_TOOL_JOYPAD {
        UnitPrivileges {
            supplementary_groups: vec!["input".to_string()],
            device_allow: vec!["/dev/input/*".to_string()],
            capabilities: Vec::new(),
        }
    } else {
        UnitPrivileges::default()
    }
}
