//! Participant systemd units and privilege projections.

use super::{
    OPT_BIN, OPT_ENV, OfficialArtifactPlan, SourceBuildArtifact, WATCHDOG_SEC,
    official_runtime_by_artifact_id,
};
use crate::supervisor::START_LIMIT_BURST;
use crate::supervisor::START_LIMIT_INTERVAL;
use anyhow::Result;
use anyhow::anyhow;
use anyhow::bail;
use phoxal::model::robot::v0::ConnectionConfig;
use phoxal_cli_core::project::launch_plan::ParticipantExecution;
use phoxal_cli_core::project::launch_plan::ParticipantLaunchRecord;
use phoxal_cli_core::project::resolver::ResolvedRobot;
use std::collections::BTreeMap;

pub(crate) fn participant_unit(
    participant: &ParticipantLaunchRecord,
    binary: &str,
    privileges: &UnitPrivileges,
) -> String {
    let id = &participant.launch.participant_id;
    let retention_order = if is_tool_execution(&participant.execution) {
        String::new()
    } else {
        format!(
            " phoxal-participant-tool-bus-{robot}.service phoxal-participant-tool-log-{robot}.service",
            robot = participant.launch.robot_id
        )
    };
    let mut unit = format!(
        "[Unit]\nDescription=Phoxal participant {id}\nAfter=network-online.target phoxal-router.service{retention_order}\nWants=network-online.target{retention_order}\nPartOf=phoxal.target\nStartLimitIntervalSec={}\nStartLimitBurst={START_LIMIT_BURST}\n\n[Service]\nType=notify\nEnvironmentFile={OPT_ENV}/{id}.env\nExecStart={OPT_BIN}/{binary}\n\nRestart=on-failure\nRestartSec=2s\nTimeoutStopSec=5s\nStateDirectory=phoxal\nWatchdogSec={WATCHDOG_SEC}s\n\nUser=phoxal\nGroup=phoxal\nNoNewPrivileges=true\n",
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

fn is_tool_execution(execution: &ParticipantExecution) -> bool {
    match execution {
        ParticipantExecution::OfficialTool { .. } => true,
        ParticipantExecution::SourceArtifact { kind, .. } => kind == "tool",
        _ => false,
    }
}

pub(crate) fn participant_unit_name(participant_id: &str) -> String {
    format!("phoxal-participant-{participant_id}.service")
}

pub(crate) fn participant_binary_name(
    participant: &ParticipantLaunchRecord,
    resolved: &ResolvedRobot,
    source_builds: &BTreeMap<String, SourceBuildArtifact>,
    official_plans: &BTreeMap<String, OfficialArtifactPlan>,
) -> Result<String> {
    match &participant.execution {
        ParticipantExecution::UserService { .. } => Ok(participant.artifact_id.clone()),
        ParticipantExecution::SourceArtifact { kind, .. } if kind == "service" => {
            Ok(format!("service-{}", participant.artifact_id))
        }
        ParticipantExecution::SourceArtifact { kind, .. } if kind == "tool" => {
            Ok(participant.artifact_id.clone())
        }
        ParticipantExecution::SourceArtifact { kind, .. } => {
            Ok(format!("{kind}-{}", participant.artifact_id))
        }
        ParticipantExecution::ComponentDriver { .. } => {
            Ok(format!("driver-{}", participant.artifact_id))
        }
        ParticipantExecution::OfficialArtifact { .. } => {
            let runtime = official_runtime_by_artifact_id(resolved, &participant.artifact_id)
                .ok_or_else(|| {
                    anyhow!(
                        "official participant {} has no resolved runtime",
                        participant.artifact_id
                    )
                })?;
            official_plans
                .get(&runtime.package)
                .map(|artifact| artifact.install_binary_name.clone())
                .ok_or_else(|| {
                    anyhow!(
                        "official participant {} has no staged artifact plan",
                        participant.artifact_id
                    )
                })
        }
        ParticipantExecution::OfficialTool { .. } => official_plans
            .get(&participant.artifact_id)
            .map(|artifact| artifact.install_binary_name.clone())
            .ok_or_else(|| {
                anyhow!(
                    "official tool participant {} has no staged artifact plan",
                    participant.artifact_id
                )
            }),
    }
    .and_then(|binary| {
        if source_builds.contains_key(&binary)
            || official_plans.contains_key(&binary)
            || !binary.is_empty()
        {
            Ok(binary)
        } else {
            bail!(
                "participant {} resolved an empty binary name",
                participant.launch.participant_id
            )
        }
    })
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct UnitPrivileges {
    pub(crate) supplementary_groups: Vec<String>,
    pub(crate) device_allow: Vec<String>,
    pub(crate) capabilities: Vec<String>,
}

pub(crate) fn unit_privileges(resolved: &ResolvedRobot, participant_id: &str) -> UnitPrivileges {
    if participant_id.starts_with("tool-joypad-") {
        return UnitPrivileges {
            supplementary_groups: vec!["input".to_string()],
            device_allow: vec!["/dev/input/*".to_string()],
            capabilities: Vec::new(),
        };
    }
    let Some(component) = resolved.robot.robot.components.get(participant_id) else {
        return UnitPrivileges::default();
    };
    let Some(driver) = component.driver.as_ref() else {
        return UnitPrivileges::default();
    };
    let mut privileges = match &driver.connection {
        ConnectionConfig::Can { bus, .. } => UnitPrivileges {
            supplementary_groups: Vec::new(),
            device_allow: Vec::new(),
            capabilities: vec!["CAP_NET_RAW".to_string()],
        }
        .with_note_device(format!("/sys/class/net/can{bus}")),
        ConnectionConfig::I2c { bus, .. } => UnitPrivileges {
            supplementary_groups: vec!["i2c".to_string()],
            device_allow: vec![format!("/dev/i2c-{bus}")],
            capabilities: Vec::new(),
        },
        ConnectionConfig::Spi { bus, chip_select } => UnitPrivileges {
            supplementary_groups: vec!["spi".to_string()],
            device_allow: vec![format!("/dev/spidev{bus}.{chip_select}")],
            capabilities: Vec::new(),
        },
        ConnectionConfig::Serial { port, .. } | ConnectionConfig::Uart { port, .. } => {
            UnitPrivileges {
                supplementary_groups: vec!["dialout".to_string()],
                device_allow: vec![port.clone()],
                capabilities: Vec::new(),
            }
        }
        ConnectionConfig::Usb { .. } => UnitPrivileges {
            supplementary_groups: vec!["plugdev".to_string(), "video".to_string()],
            device_allow: Vec::new(),
            capabilities: Vec::new(),
        },
        ConnectionConfig::Gpio { chip, .. } => UnitPrivileges {
            supplementary_groups: vec!["gpio".to_string()],
            device_allow: vec![if chip.starts_with('/') {
                chip.clone()
            } else {
                format!("/dev/{chip}")
            }],
            capabilities: Vec::new(),
        },
    };
    privileges.sort_dedup();
    privileges
}

impl UnitPrivileges {
    fn with_note_device(self, _device: String) -> Self {
        self
    }

    fn sort_dedup(&mut self) {
        self.supplementary_groups.sort();
        self.supplementary_groups.dedup();
        self.device_allow.sort();
        self.device_allow.dedup();
        self.capabilities.sort();
        self.capabilities.dedup();
    }
}
